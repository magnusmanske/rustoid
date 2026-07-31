//! Wikitext token types.
//!
//! The tokenizer produces a flat stream of these tokens from raw wikitext.
//! The tree builder (Phase 2) converts this stream into a nested AST.

use std::fmt;

/// A single token from the wikitext tokenizer.
#[derive(Debug, Clone, PartialEq)]
pub enum WikitextToken {
    /// Plain text content.
    Text(String),

    // ---- Inline formatting ----
    /// Opening bold marker (`'''`).
    BoldOpen,
    /// Closing bold marker (`'''`).
    BoldClose,
    /// Opening italic marker (`''`).
    ItalicOpen,
    /// Closing italic marker (`''`).
    ItalicClose,

    // ---- Links ----
    /// Opening of a wikilink (`[[`).
    WikilinkOpen,
    /// Pipe separator in a wikilink (`|`).
    WikilinkPipe,
    /// Closing of a wikilink (`]]`).
    WikilinkClose,
    /// Opening of an external link (`[http://...`).
    ExtLinkOpen(String),
    /// Space/separator in an external link.
    ExtLinkSep,
    /// Closing of an external link (`]`).
    ExtLinkClose,

    // ---- Transclusion ----
    /// Opening of a template (`{{Name`). Contains the template name.
    TemplateOpen(String),
    /// Pipe separator in a template parameter.
    TemplatePipe,
    /// Closing of a template (`}}`).
    TemplateClose,
    /// Opening of a template argument reference (`{{{name`).
    TplArgOpen(String),
    /// Pipe separator in a template argument (for default value).
    TplArgPipe,
    /// Closing of a template argument reference (`}}}`).
    TplArgClose,

    // ---- Parser functions ----
    /// Opening of a parser function (`{{#name:`).
    ParserFnOpen(String),
    /// Colon separator in a parser function.
    ParserFnColon,
    /// Closing of a parser function (`}}`).
    ParserFnClose,

    // ---- Magic words ----
    /// A magic word (behavior switch like `__TOC__`, or variable like `{{PAGENAME}}`).
    MagicWord(String),

    // ---- Comments ----
    /// An HTML comment (`<!-- ... -->`).
    Comment(String),

    // ---- HTML tags ----
    /// Opening HTML tag (`<div class="foo">`).
    HtmlTagOpen(String, Vec<(String, String)>),
    /// Closing HTML tag (`</div>`).
    HtmlTagClose(String),
    /// Self-closing HTML tag (`<br/>`, `<ref name="x"/>`).
    SelfClosingTag(String, Vec<(String, String)>),

    // ---- Nowiki ----
    /// Content inside a `<nowiki>` block (no further parsing).
    NowikiContent(String),

    // ---- Headings ----
    /// Opening of a heading section marker (`==`, `===`, etc.). The u8 is the level (2-6).
    HeadingOpen(u8),
    /// Closing of a heading section marker.
    HeadingClose,

    // ---- Horizontal rule ----
    /// Horizontal rule (`----`).
    Hr,

    // ---- Lists ----
    /// Start of a list item. `char` is `*`, `#`, `;`, or `:`. `u8` is the nesting depth.
    ListItem(char, u8),

    // ---- Tables ----
    /// Start of a wikitext table (`{|`).
    TableOpen(Vec<(String, String)>),
    /// Table row separator (`|-`).
    TableRow,
    /// Table cell separator (`|`, `||`, `!`, `!!`).
    TableCell,
    /// Table caption marker (`|+`).
    TableCaption,
    /// End of a wikitext table (`|}`).
    TableClose,

    // ---- Whitespace / structure ----
    /// A single explicit newline in the source.
    Newline,
    /// A double newline (paragraph break).
    ParagraphBreak,

    // ---- Redirect ----
    /// A redirect directive (`#REDIRECT [[Target]]`).
    Redirect(String),

    // ---- Extension tags ----
    /// An extension tag (e.g. `<gallery>`, `<poem>`), including its body.
    ExtensionTag {
        name: String,
        attrs: Vec<(String, String)>,
        body: String,
    },

    // ---- Annotations ----
    /// Opening annotation tag (e.g. `<dummyanno>`).
    AnnotationOpen(String, Vec<(String, String)>),
    /// Closing annotation tag (e.g. `</dummyanno>`).
    AnnotationClose(String),

    /// End of input.
    EOF,
}

impl WikitextToken {
    /// Returns `true` if this token represents inline content (text, formatting, etc.).
    pub fn is_inline(&self) -> bool {
        matches!(
            self,
            WikitextToken::Text(_)
                | WikitextToken::BoldOpen
                | WikitextToken::BoldClose
                | WikitextToken::ItalicOpen
                | WikitextToken::ItalicClose
                | WikitextToken::WikilinkOpen
                | WikitextToken::WikilinkPipe
                | WikitextToken::WikilinkClose
                | WikitextToken::ExtLinkOpen(_)
                | WikitextToken::ExtLinkSep
                | WikitextToken::ExtLinkClose
                | WikitextToken::MagicWord(_)
                | WikitextToken::Comment(_)
                | WikitextToken::NowikiContent(_)
        )
    }

    /// Returns `true` if this token starts a block-level construct.
    pub fn is_block_start(&self) -> bool {
        matches!(
            self,
            WikitextToken::HeadingOpen(_)
                | WikitextToken::Hr
                | WikitextToken::ListItem(_, _)
                | WikitextToken::TableOpen(_)
                | WikitextToken::Redirect(_)
        )
    }
}

impl fmt::Display for WikitextToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WikitextToken::Text(s) => write!(f, "Text({s:?})"),
            WikitextToken::BoldOpen => write!(f, "BoldOpen"),
            WikitextToken::BoldClose => write!(f, "BoldClose"),
            WikitextToken::ItalicOpen => write!(f, "ItalicOpen"),
            WikitextToken::ItalicClose => write!(f, "ItalicClose"),
            WikitextToken::WikilinkOpen => write!(f, "WikilinkOpen"),
            WikitextToken::WikilinkPipe => write!(f, "WikilinkPipe"),
            WikitextToken::WikilinkClose => write!(f, "WikilinkClose"),
            WikitextToken::ExtLinkOpen(u) => write!(f, "ExtLinkOpen({u})"),
            WikitextToken::ExtLinkSep => write!(f, "ExtLinkSep"),
            WikitextToken::ExtLinkClose => write!(f, "ExtLinkClose"),
            WikitextToken::TemplateOpen(n) => write!(f, "TemplateOpen({n})"),
            WikitextToken::TemplatePipe => write!(f, "TemplatePipe"),
            WikitextToken::TemplateClose => write!(f, "TemplateClose"),
            WikitextToken::TplArgOpen(n) => write!(f, "TplArgOpen({n})"),
            WikitextToken::TplArgPipe => write!(f, "TplArgPipe"),
            WikitextToken::TplArgClose => write!(f, "TplArgClose"),
            WikitextToken::ParserFnOpen(n) => write!(f, "ParserFnOpen({n})"),
            WikitextToken::ParserFnColon => write!(f, "ParserFnColon"),
            WikitextToken::ParserFnClose => write!(f, "ParserFnClose"),
            WikitextToken::MagicWord(w) => write!(f, "MagicWord({w})"),
            WikitextToken::Comment(c) => write!(f, "Comment({c:?})"),
            WikitextToken::HtmlTagOpen(n, _) => write!(f, "HtmlTagOpen({n})"),
            WikitextToken::HtmlTagClose(n) => write!(f, "HtmlTagClose({n})"),
            WikitextToken::SelfClosingTag(n, _) => write!(f, "SelfClosingTag({n})"),
            WikitextToken::NowikiContent(s) => write!(f, "NowikiContent({s:?})"),
            WikitextToken::HeadingOpen(l) => write!(f, "HeadingOpen({l})"),
            WikitextToken::HeadingClose => write!(f, "HeadingClose"),
            WikitextToken::Hr => write!(f, "Hr"),
            WikitextToken::ListItem(c, d) => write!(f, "ListItem({c}, {d})"),
            WikitextToken::TableOpen(_) => write!(f, "TableOpen"),
            WikitextToken::TableRow => write!(f, "TableRow"),
            WikitextToken::TableCell => write!(f, "TableCell"),
            WikitextToken::TableCaption => write!(f, "TableCaption"),
            WikitextToken::TableClose => write!(f, "TableClose"),
            WikitextToken::Newline => write!(f, "Newline"),
            WikitextToken::ParagraphBreak => write!(f, "ParagraphBreak"),
            WikitextToken::Redirect(t) => write!(f, "Redirect({t})"),
            WikitextToken::ExtensionTag { name, .. } => write!(f, "ExtensionTag({name})"),
            WikitextToken::AnnotationOpen(n, _) => write!(f, "AnnotationOpen({n})"),
            WikitextToken::AnnotationClose(n) => write!(f, "AnnotationClose({n})"),
            WikitextToken::EOF => write!(f, "EOF"),
        }
    }
}
