/// Parser configuration options, mirroring Parsoid's option set.
///
/// See [Parsoid documentation](https://www.mediawiki.org/wiki/Parsoid) for details.
use serde::{Deserialize, Serialize};

/// Which parsing mode to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParseMode {
    /// Wikitext → HTML conversion (default).
    #[default]
    Wt2Html,
    /// HTML → Wikitext conversion.
    Html2Wt,
    /// Wikitext → HTML → Wikitext round-trip test.
    Wt2Wt,
    /// Selective serialization: given original wikitext, original HTML, and modified
    /// HTML, produce modified wikitext.
    Selser,
}

/// Offset type for DSR (DOM Source Range) byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OffsetType {
    /// Raw byte offsets in the UTF-8 source.
    #[default]
    Byte,
    /// UCS-2 code unit offsets (like JavaScript).
    Ucs2,
    /// Unicode scalar value offsets.
    Char,
}

/// PageBundle mode — whether to separate data-mw/data-parsoid into a JSON envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageBundleMode {
    /// Embed data attributes inline in the HTML (default).
    #[default]
    Inline,
    /// Return a JSON page bundle with separate HTML and data sections.
    Bundle,
}

/// Top-level parser options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParserOptions {
    /// Parsing direction/mode.
    #[serde(default)]
    pub mode: ParseMode,

    /// Output only the document body (omit `<html>`, `<head>`, `<body>` wrappers).
    #[serde(default)]
    pub body_only: bool,

    /// Wrap sections in `<section>` tags (for mobile/section editing).
    #[serde(default)]
    pub wrap_sections: bool,

    /// Include section edit anchors.
    #[serde(default)]
    pub section_anchors: bool,

    /// Output content version string (e.g. `"2.4.0"`, `"999.0.0"`).
    #[serde(default = "default_content_version")]
    pub output_content_version: String,

    /// DSR offset type.
    #[serde(default)]
    pub offset_type: OffsetType,

    /// Page bundle mode.
    #[serde(default)]
    pub page_bundle: PageBundleMode,

    /// Enable lint error reporting.
    #[serde(default)]
    pub linting: bool,

    /// Process annotation tags (`<dummyanno>`, etc.).
    #[serde(default)]
    pub annotations: bool,

    /// The language code of the wiki (e.g. `"en"`).
    #[serde(default = "default_language")]
    pub language: String,

    /// The page title being parsed (used for `{{PAGENAME}}` etc.).
    #[serde(default)]
    pub page_title: String,

    /// The oldid revision being parsed (for time-dependent magic words).
    #[serde(default)]
    pub oldid: Option<u64>,

    /// The wikitext input for HTML→wikitext or selser modes.
    #[serde(default)]
    pub original_wikitext: Option<String>,

    /// The original HTML for selser mode.
    #[serde(default)]
    pub original_html: Option<String>,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self {
            mode: ParseMode::default(),
            body_only: false,
            wrap_sections: false,
            section_anchors: false,
            output_content_version: default_content_version(),
            offset_type: OffsetType::default(),
            page_bundle: PageBundleMode::default(),
            linting: false,
            annotations: false,
            language: default_language(),
            page_title: String::new(),
            oldid: None,
            original_wikitext: None,
            original_html: None,
        }
    }
}

impl ParserOptions {
    /// Create options for a simple wikitext→HTML parse of a given page.
    pub fn for_page(title: impl Into<String>) -> Self {
        Self {
            page_title: title.into(),
            ..Default::default()
        }
    }

    /// Create options for an HTML→wikitext round-trip.
    pub fn for_html2wt(title: impl Into<String>, original_wikitext: impl Into<String>) -> Self {
        Self {
            mode: ParseMode::Html2Wt,
            page_title: title.into(),
            original_wikitext: Some(original_wikitext.into()),
            ..Default::default()
        }
    }
}

fn default_content_version() -> String {
    "2.4.0".to_string()
}

fn default_language() -> String {
    "en".to_string()
}
