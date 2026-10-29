//! Pygments' `WikitextLexer`, ported rule for rule.
//!
//! The lexer is Pygments' (`pygments.lexers.markup.WikitextLexer`), which is
//! what the SyntaxHighlight extension runs; the rule order and every pattern
//! below are transcribed from it, because the token boundaries are observable in
//! the served HTML.
//!
//! Not ported, and therefore served as plain text where the service highlights
//! them:
//!
//! - language-converter markup (`-{…}-`),
//! - the nested `<syntaxhighlight>`/`<source>`/`<score>` bodies, whose content is
//!   lexed by another lexer,
//! - `<math>`/`<chem>`/`<ce>` (TeX) and `<templatedata>` (JSON), which delegate
//!   to a Python lexer.
//!
//! Everything else — headings, lists, tables' inline parts, templates and their
//! arguments, parser functions, wikilinks, media links, external links, HTML and
//! parser tags, comments and quotes — is lexed as Pygments does. Table lines are
//! the one exception inside that list: their attribute scan needs a
//! negative lookahead Pygments uses per character, so a table is lexed as
//! ordinary text (see `table` in the gap list of ONLINE-PARITY.md).

use std::collections::HashMap;
use std::sync::OnceLock;

use super::lexer::{Action, GroupAction, Guard, Matcher, Next, Rule};

/// Pygments' `title_char` class body (a-z comes from the `(?i)` flag).
const TC: &str = r#" %!"$&'()*,\-./0-9:;=?@A-Z\\\^_`~+\u{0080}-\u{FFFF}"#;
/// `title_char` without `/`, for a wikilink's namespace.
const TC_NS: &str = r#" %!"$&'()*,\-.:;=?@A-Z\\\^_`~+\u{0080}-\u{FFFF}"#;

const PROTOCOLS: &[&str] = &[
    "bitcoin:",
    "ftp://",
    "ftps://",
    "geo:",
    "git://",
    "gopher://",
    "http://",
    "https://",
    "irc://",
    "ircs://",
    "magnet:",
    "mailto:",
    "mms://",
    "news:",
    "nntp://",
    "redis://",
    "sftp://",
    "sip:",
    "sips:",
    "sms:",
    "ssh://",
    "svn://",
    "tel:",
    "telnet://",
    "urn:",
    "worldwind://",
    "xmpp:",
    "//",
];

const DOUBLE_SLASHES_I: &[&str] = &[
    "__FORCETOC__",
    "__NOCONTENTCONVERT__",
    "__NOCC__",
    "__NOEDITSECTION__",
    "__NOGALLERY__",
    "__NOTITLECONVERT__",
    "__NOTC__",
    "__NOTOC__",
    "__TOC__",
];

const DOUBLE_SLASHES: &[&str] = &[
    "__EXPECTUNUSEDCATEGORY__",
    "__HIDDENCAT__",
    "__INDEX__",
    "__NEWSECTIONLINK__",
    "__NOINDEX__",
    "__NONEWSECTIONLINK__",
    "__STATICREDIRECT__",
    "__NOGLOBAL__",
    "__DISAMBIG__",
    "__EXPECTED_UNCONNECTED_PAGE__",
];

const MAGIC_VARS_I: &[&str] = &[
    "ARTICLEPATH",
    "INT",
    "PAGEID",
    "SCRIPTPATH",
    "SERVER",
    "SERVERNAME",
    "STYLEPATH",
];

const MAGIC_VARS: &[&str] = &[
    "!",
    "=",
    "BASEPAGENAME",
    "BASEPAGENAMEE",
    "CASCADINGSOURCES",
    "CONTENTLANGUAGE",
    "CONTENTLANG",
    "CURRENTDAY",
    "CURRENTDAY2",
    "CURRENTDAYNAME",
    "CURRENTDOW",
    "CURRENTHOUR",
    "CURRENTMONTH",
    "CURRENTMONTH2",
    "CURRENTMONTH1",
    "CURRENTMONTHABBREV",
    "CURRENTMONTHNAME",
    "CURRENTMONTHNAMEGEN",
    "CURRENTTIME",
    "CURRENTTIMESTAMP",
    "CURRENTVERSION",
    "CURRENTWEEK",
    "CURRENTYEAR",
    "DIRECTIONMARK",
    "DIRMARK",
    "FULLPAGENAME",
    "FULLPAGENAMEE",
    "LOCALDAY",
    "LOCALDAY2",
    "LOCALDAYNAME",
    "LOCALDOW",
    "LOCALHOUR",
    "LOCALMONTH",
    "LOCALMONTH2",
    "LOCALMONTH1",
    "LOCALMONTHABBREV",
    "LOCALMONTHNAME",
    "LOCALMONTHNAMEGEN",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "LOCALWEEK",
    "LOCALYEAR",
    "NAMESPACE",
    "NAMESPACEE",
    "NAMESPACENUMBER",
    "NUMBEROFACTIVEUSERS",
    "NUMBEROFADMINS",
    "NUMBEROFARTICLES",
    "NUMBEROFEDITS",
    "NUMBEROFFILES",
    "NUMBEROFPAGES",
    "NUMBEROFUSERS",
    "PAGELANGUAGE",
    "PAGENAME",
    "PAGENAMEE",
    "REVISIONDAY",
    "REVISIONDAY2",
    "REVISIONID",
    "REVISIONMONTH",
    "REVISIONMONTH1",
    "REVISIONSIZE",
    "REVISIONTIMESTAMP",
    "REVISIONUSER",
    "REVISIONYEAR",
    "ROOTPAGENAME",
    "ROOTPAGENAMEE",
    "SITENAME",
    "SUBJECTPAGENAME",
    "ARTICLEPAGENAME",
    "SUBJECTPAGENAMEE",
    "ARTICLEPAGENAMEE",
    "SUBJECTSPACE",
    "ARTICLESPACE",
    "SUBJECTSPACEE",
    "ARTICLESPACEE",
    "SUBPAGENAME",
    "SUBPAGENAMEE",
    "TALKPAGENAME",
    "TALKPAGENAMEE",
    "TALKSPACE",
    "TALKSPACEE",
];

const PARSER_FUNCTIONS_I: &[&str] = &[
    "ANCHORENCODE",
    "BIDI",
    "CANONICALURL",
    "CANONICALURLE",
    "FILEPATH",
    "FORMATNUM",
    "FULLURL",
    "FULLURLE",
    "GENDER",
    "GRAMMAR",
    "INT",
    r"\#LANGUAGE",
    "LC",
    "LCFIRST",
    "LOCALURL",
    "LOCALURLE",
    "NS",
    "NSE",
    "PADLEFT",
    "PADRIGHT",
    "PAGEID",
    "PLURAL",
    "UC",
    "UCFIRST",
    "URLENCODE",
];

const PARSER_FUNCTIONS: &[&str] = &[
    "BASEPAGENAME",
    "BASEPAGENAMEE",
    "CASCADINGSOURCES",
    "DEFAULTSORT",
    "DEFAULTSORTKEY",
    "DEFAULTCATEGORYSORT",
    "FULLPAGENAME",
    "FULLPAGENAMEE",
    "NAMESPACE",
    "NAMESPACEE",
    "NAMESPACENUMBER",
    "NUMBERINGROUP",
    "NUMINGROUP",
    "NUMBEROFACTIVEUSERS",
    "NUMBEROFADMINS",
    "NUMBEROFARTICLES",
    "NUMBEROFEDITS",
    "NUMBEROFFILES",
    "NUMBEROFPAGES",
    "NUMBEROFUSERS",
    "PAGENAME",
    "PAGENAMEE",
    "PAGESINCATEGORY",
    "PAGESINCAT",
    "PAGESIZE",
    "PROTECTIONEXPIRY",
    "PROTECTIONLEVEL",
    "REVISIONDAY",
    "REVISIONDAY2",
    "REVISIONID",
    "REVISIONMONTH",
    "REVISIONMONTH1",
    "REVISIONTIMESTAMP",
    "REVISIONUSER",
    "REVISIONYEAR",
    "ROOTPAGENAME",
    "ROOTPAGENAMEE",
    "SUBJECTPAGENAME",
    "ARTICLEPAGENAME",
    "SUBJECTPAGENAMEE",
    "ARTICLEPAGENAMEE",
    "SUBJECTSPACE",
    "ARTICLESPACE",
    "SUBJECTSPACEE",
    "ARTICLESPACEE",
    "SUBPAGENAME",
    "SUBPAGENAMEE",
    "TALKPAGENAME",
    "TALKPAGENAMEE",
    "TALKSPACE",
    "TALKSPACEE",
    "INT",
    "DISPLAYTITLE",
    "PAGESINNAMESPACE",
    "PAGESINNS",
];

const HTML_TAGS: &[&str] = &[
    "abbr",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "data",
    "dd",
    "del",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "ins",
    "kbd",
    "li",
    "link",
    "mark",
    "meta",
    "ol",
    "p",
    "q",
    "rb",
    "rp",
    "rt",
    "rtc",
    "ruby",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "td",
    "th",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

const PARSER_TAGS: &[&str] = &[
    "graph",
    "charinsert",
    "rss",
    "chem",
    "categorytree",
    "nowiki",
    "inputbox",
    "math",
    "hiero",
    "score",
    "pre",
    "ref",
    "translate",
    "imagemap",
    "templatestyles",
    "languages",
    "noinclude",
    "mapframe",
    "section",
    "poem",
    "syntaxhighlight",
    "includeonly",
    "tvar",
    "onlyinclude",
    "templatedata",
    "langconvert",
    "timeline",
    "dynamicpagelist",
    "gallery",
    "maplink",
    "ce",
    "references",
];

/// The relex stack `using(this, state=['root', 'attr'])` names.
const ATTR: &[&str] = &["root", "attr"];

/// The states this port implements.
pub(super) const STATE_NAMES: &[&str] = &[
    "root",
    "list",
    "inline",
    "replaceable",
    "parameter-inner",
    "template-begin-space",
    "template-name",
    "template-inner",
    "wikilink-name",
    "wikilink-inner",
    "medialink-inner",
    "extlink-inner",
    "quote-common",
    "inline-italic",
    "inline-bold",
    "inline-bold-italic",
    "inline-italic-bold",
    "text",
    "text-italic",
    "text-bold",
    "text-bold-italic",
    "entity",
    "dt",
    "table",
    "table-header",
    "attr",
    "attr-val-0",
    "attr-val-1",
    "attr-val-2",
    "tag-inner-ordinary",
    "tag-inner",
    "tag-attr",
    "tag-attr-val-0",
    "tag-attr-val-1",
    "tag-attr-val-2",
    "tag-nowiki",
    "tag-pre",
    "tag-categorytree",
    "tag-dynamicpagelist",
    "tag-hiero",
    "tag-inputbox",
    "tag-imagemap",
    "tag-charinsert",
    "tag-timeline",
    "tag-gallery",
    "tag-graph",
    "tag-rss",
];

const LINK_ADDRESS: &str = concat!(
    r#"(?:[0-9.]+|\[[0-9a-f:.]+\]|[^\x00-\x20"<>\[\]\x7F\xA0"#,
    r"\u{1680}\u{2000}-\u{200A}\u{202F}\u{205F}\u{3000}\u{FFFD}])"
);

const LINK_CHAR: &str = concat!(
    r#"[^\x00-\x20"<>\[\]\x7F\xA0"#,
    r"\u{1680}\u{2000}-\u{200A}\u{202F}\u{205F}\u{3000}\u{FFFD}]"
);

const NBSP: &str = concat!(
    r"(?:\t|&nbsp;|&#0*160;|&#[Xx]0*[Aa]0;|[ \x{A0}",
    r"\u{1680}\u{2000}-\u{200A}\u{202F}\u{205F}\u{3000}])"
);

/// Build a pattern once, keeping it for the process lifetime.
fn pat(slot: &'static OnceLock<String>, make: impl FnOnce() -> String) -> &'static str {
    slot.get_or_init(make).as_str()
}

/// Join a word list into an alternation, longest word first.
fn joined(words: &[&str]) -> String {
    let mut sorted: Vec<&str> = words.to_vec();
    sorted.sort_by_key(|w| std::cmp::Reverse(w.len()));
    sorted.join("|")
}

static DOUBLE_I: OnceLock<String> = OnceLock::new();
static DOUBLE: OnceLock<String> = OnceLock::new();
static RAW_URL: OnceLock<String> = OnceLock::new();
static ISBN: OnceLock<String> = OnceLock::new();
static MEDIA: OnceLock<String> = OnceLock::new();
static WIKILINK_CLOSED: OnceLock<String> = OnceLock::new();
static WIKILINK_OPEN: OnceLock<String> = OnceLock::new();
static EXTLINK: OnceLock<String> = OnceLock::new();
static HTML_OPEN: OnceLock<String> = OnceLock::new();
static HTML_CLOSE: OnceLock<String> = OnceLock::new();
static PARSER_OPEN: OnceLock<String> = OnceLock::new();
static PARSER_CLOSE: OnceLock<String> = OnceLock::new();
static MAGIC_I: OnceLock<String> = OnceLock::new();
static MAGIC: OnceLock<String> = OnceLock::new();
static MAGIC_LINK: OnceLock<String> = OnceLock::new();
static PROTOCOL_ALT: OnceLock<String> = OnceLock::new();
static PF_I: OnceLock<String> = OnceLock::new();
static PF: OnceLock<String> = OnceLock::new();
static NS_PREFIX: OnceLock<String> = OnceLock::new();

fn magic_var(slot: &'static OnceLock<String>, user_i: bool) -> &'static str {
    pat(slot, || {
        let (flag, body) = if user_i {
            ("(?i)", joined(MAGIC_VARS_I))
        } else {
            ("", joined(MAGIC_VARS))
        };
        format!(r"{flag}(\{{\{{)(\s*)({body})(\s*)(\}}\}})")
    })
}

fn wikilink(slot: &'static OnceLock<String>, tail: &'static str) -> &'static str {
    pat(slot, || {
        format!(
            r"(?i)(\[\[)(?:([{TC_NS}]*)(:))?((?:[{TC}]|\{{\{{2,3\}}[^{{}}]*?\}}\{{2,3\}}|<!--[\s\S]*?-->)*?)(?:(\#)([{TC}#]*?))?{tail}"
        )
    })
}

/// The compiled states, built once.
pub fn compiled() -> &'static HashMap<&'static str, Vec<super::lexer::Compiled>> {
    static TABLE: OnceLock<HashMap<&'static str, Vec<super::lexer::Compiled>>> = OnceLock::new();
    TABLE.get_or_init(|| super::lexer::compile(rules))
}

fn include(out: &mut Vec<Rule>, state: &str) {
    out.extend(rules(state));
}

pub(super) fn rules(state: &str) -> Vec<Rule> {
    match state {
        "root" => {
            let mut v = vec![
                Rule {
                    matcher: Matcher::Subheading,
                    guard: Guard::None,
                    action: Action::Class("gu"),
                    next: Next::Stay,
                },
                Rule {
                    matcher: Matcher::Heading,
                    guard: Guard::None,
                    action: Action::Class("gh"),
                    next: Next::Stay,
                },
                Rule::rx(pat(&DOUBLE_I, || joined(DOUBLE_SLASHES_I)), "fm"),
                Rule::rx(pat(&DOUBLE, || joined(DOUBLE_SLASHES)), "fm"),
                Rule::rx(
                    pat(&RAW_URL, || {
                        format!("(?i)\\b(?:{}){LINK_ADDRESS}{LINK_CHAR}*", joined(PROTOCOLS))
                    }),
                    "nl",
                ),
                Rule::rx(
                    pat(&MAGIC_LINK, || format!(r"\b(?:RFC|PMID){NBSP}+[0-9]+\b")),
                    "fm",
                ),
                Rule::rx(
                    pat(&ISBN, || {
                        format!(
                            r"\bISBN {NBSP}(?:97[89](?:-|{NBSP})?)?(?:[0-9](?:-|{NBSP})?){{9}}[0-9Xx]\b"
                        )
                    }),
                    "fm",
                ),
            ];
            include(&mut v, "list");
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "list" => vec![
            Rule::rx_next(r"^;", "k", Next::Push("dt")),
            Rule::rx(r"^[#:*]+", "k"),
            Rule::rx(r"^-{4,}", "k"),
        ],
        "inline" => {
            let mut v = vec![
                Rule::rx(r"~{3,5}", "k"),
                Rule::rx(r"&\S*?;", "ni"),
                Rule::groups(
                    r"('')(''')",
                    &[GroupAction::Tok("ge", 1), GroupAction::Tok("ges", 2)],
                    Next::Push("inline-italic-bold"),
                )
                .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"'''", "gs", Next::Push("inline-bold")).guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"''", "ge", Next::Push("inline-italic")).guard(Guard::NotAtEnd("'")),
            ];
            include(&mut v, "replaceable");
            v.push(
                Rule::groups(
                    pat(&MEDIA, || {
                        format!(
                            r"(?i)(\[\[)(File|Image)(:)((?:[{TC}]|\{{\{{2,3\}}[^{{}}]*?\}}\{{2,3\}}|<!--[\s\S]*?-->)*)(?:(\#)([{TC}#]*?))?"
                        )
                    }),
                    &[
                        GroupAction::Tok("p", 1),
                        GroupAction::Tok("nn", 2),
                        GroupAction::Tok("p", 3),
                        GroupAction::Relex(4, &["root", "wikilink-name"]),
                        GroupAction::Tok("p", 5),
                        GroupAction::Tok("nl", 6),
                    ],
                    Next::Push("medialink-inner"),
                ),
            );
            v.push(
                Rule::groups(
                    wikilink(&WIKILINK_CLOSED, r"(\]\])"),
                    &[
                        GroupAction::Tok("p", 1),
                        GroupAction::Tok("nn", 2),
                        GroupAction::Tok("p", 3),
                        GroupAction::Relex(4, &["root", "wikilink-name"]),
                        GroupAction::Tok("p", 5),
                        GroupAction::Tok("nl", 6),
                        GroupAction::Tok("p", 7),
                    ],
                    Next::Stay,
                )
                .guard(Guard::NotAt(2, pat(&PROTOCOL_ALT, || joined(PROTOCOLS)))),
            );
            v.push(
                Rule::groups(
                    wikilink(&WIKILINK_OPEN, r"(\|)"),
                    &[
                        GroupAction::Tok("p", 1),
                        GroupAction::Tok("nn", 2),
                        GroupAction::Tok("p", 3),
                        GroupAction::Relex(4, &["root", "wikilink-name"]),
                        GroupAction::Tok("p", 5),
                        GroupAction::Tok("nl", 6),
                        GroupAction::Tok("p", 7),
                    ],
                    Next::Push("wikilink-inner"),
                )
                .guard(Guard::NotAt(2, pat(&PROTOCOL_ALT, || joined(PROTOCOLS)))),
            );
            v.push(Rule::groups(
                pat(&EXTLINK, || {
                    format!(
                        "(?i)(\\[)((?:{}){LINK_ADDRESS}{LINK_CHAR}*)(\\s*)",
                        joined(PROTOCOLS)
                    )
                }),
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("nl", 2),
                    GroupAction::Tok("w", 3),
                ],
                Next::Push("extlink-inner"),
            ));
            v.push(Rule {
                matcher: Matcher::TableStart,
                guard: Guard::None,
                action: Action::Groups(&[
                    GroupAction::Tok("k", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("p", 3),
                    GroupAction::Relex(4, ATTR),
                ]),
                next: Next::Push("table"),
            });
            v.push(Rule::groups(
                pat(&HTML_OPEN, || format!("(?i)(<)({})\\b", joined(HTML_TAGS))),
                &[GroupAction::Tok("p", 1), GroupAction::Tok("nt", 2)],
                Next::Push("tag-inner-ordinary"),
            ));
            v.push(Rule::groups(
                pat(&HTML_CLOSE, || {
                    format!("(?i)(</)({})\\b(\\s*)(>)", joined(HTML_TAGS))
                }),
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("nt", 2),
                    GroupAction::Tok("w", 3),
                    GroupAction::Tok("p", 4),
                ],
                Next::Stay,
            ));
            for (pattern, next_state) in [
                (r"(?i)(<)(nowiki)\b", "tag-nowiki"),
                (r"(?i)(<)(pre)\b", "tag-pre"),
                (r"(?i)(<)(categorytree)\b", "tag-categorytree"),
                (r"(?i)(<)(hiero)\b", "tag-hiero"),
                (r"(?i)(<)(charinsert)\b", "tag-charinsert"),
                (r"(?i)(<)(gallery)\b", "tag-gallery"),
                (r"(?i)(<)(dynamicpagelist)\b", "tag-dynamicpagelist"),
                (r"(?i)(<)(inputbox)\b", "tag-inputbox"),
                (r"(?i)(<)(rss)\b", "tag-rss"),
                (r"(?i)(<)(imagemap)\b", "tag-imagemap"),
            ] {
                v.push(Rule::groups(
                    pattern,
                    &[GroupAction::Tok("p", 1), GroupAction::Tok("nt", 2)],
                    Next::Push2(next_state, "tag-inner"),
                ));
            }
            v.push(Rule::groups(
                pat(&PARSER_OPEN, || {
                    format!("(?i)(<)({})\\b", joined(PARSER_TAGS))
                }),
                &[GroupAction::Tok("p", 1), GroupAction::Tok("nt", 2)],
                Next::Push("tag-inner-ordinary"),
            ));
            v.push(Rule::groups(
                pat(&PARSER_CLOSE, || {
                    format!("(?i)(</)({})\\b(\\s*)(>)", joined(PARSER_TAGS))
                }),
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("nt", 2),
                    GroupAction::Tok("w", 3),
                    GroupAction::Tok("p", 4),
                ],
                Next::Stay,
            ));
            v
        }
        "replaceable" => vec![
            Rule::rx(r"<!--[\s\S]*?(?:-->|\z)", "cm"),
            Rule::parameter(Next::Push("parameter-inner")),
            Rule::groups(
                magic_var(&MAGIC_I, true),
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("nf", 3),
                    GroupAction::Tok("w", 4),
                    GroupAction::Tok("p", 5),
                ],
                Next::Stay,
            ),
            Rule::groups(
                magic_var(&MAGIC, false),
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("nf", 3),
                    GroupAction::Tok("w", 4),
                    GroupAction::Tok("p", 5),
                ],
                Next::Stay,
            ),
            Rule::rx_next(r"\{\{", "p", Next::Push("template-begin-space")),
            Rule::groups(
                r"(?i)(<)(tvar)\b(\|)([^>]*?)(>)",
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("nt", 2),
                    GroupAction::Tok("p", 3),
                    GroupAction::Tok("s", 4),
                    GroupAction::Tok("p", 5),
                ],
                Next::Stay,
            ),
            Rule::rx_next(r"</>", "p", Next::Pop),
            Rule::groups(
                r"(?i)(<)(tvar)\b",
                &[GroupAction::Tok("p", 1), GroupAction::Tok("nt", 2)],
                Next::Push("tag-inner-ordinary"),
            ),
            Rule::groups(
                r"(?i)(</)(tvar)\b(\s*)(>)",
                &[
                    GroupAction::Tok("p", 1),
                    GroupAction::Tok("nt", 2),
                    GroupAction::Tok("w", 3),
                    GroupAction::Tok("p", 4),
                ],
                Next::Stay,
            ),
        ],
        "parameter-inner" => {
            let mut v = vec![
                Rule::rx_next(r"\}\}\}", "p", Next::Pop),
                Rule::rx(r"\|", "p"),
            ];
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "template-begin-space" => vec![
            Rule::rx(r"<!--[\s\S]*?(?:-->|\z)", "cm"),
            Rule::rx(r"\s+", "w"),
            Rule::groups(
                pat(&PF_I, || {
                    format!("(?i)(\\#[{TC}]*?|{})(:)", joined(PARSER_FUNCTIONS_I))
                }),
                &[GroupAction::Tok("nf", 1), GroupAction::Tok("p", 2)],
                Next::PopPush("template-inner"),
            ),
            Rule::groups(
                pat(&PF, || format!("({})(:)", joined(PARSER_FUNCTIONS))),
                &[GroupAction::Tok("nf", 1), GroupAction::Tok("p", 2)],
                Next::PopPush("template-inner"),
            ),
            Rule::groups(
                pat(&NS_PREFIX, || format!("(?i)([{TC}]*?)(:)")),
                &[GroupAction::Tok("nn", 1), GroupAction::Tok("p", 2)],
                Next::PopPush("template-name"),
            ),
            Rule::nothing("", Next::PopPush("template-name")),
        ],
        "template-name" => {
            let mut v = vec![
                Rule::groups(
                    r"(\s*?)(\|)",
                    &[GroupAction::Tok("", 1), GroupAction::Tok("p", 2)],
                    Next::PopPush("template-inner"),
                ),
                Rule::rx_next(r"\}\}", "p", Next::Pop),
                Rule::rx_next(r"\n", "", Next::Pop),
            ];
            include(&mut v, "replaceable");
            v.extend(text_rules("nt"));
            v
        }
        "template-inner" => {
            let mut v = vec![
                Rule::rx_next(r"\}\}", "p", Next::Pop),
                Rule::rx(r"\|", "p"),
                Rule::label(Next::Stay),
            ];
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "wikilink-name" => {
            let mut v = Vec::new();
            include(&mut v, "replaceable");
            v.push(Rule::rx(r"[^{<]+", "nt"));
            v.push(Rule::rx(r"(?s).", "nt"));
            v
        }
        "wikilink-inner" => {
            let mut v = vec![
                Rule::rx_next(r"", "p", Next::Pop).guard(Guard::AtEnd(r"\[\[")),
                Rule::rx_next(r"\]\]", "p", Next::Pop),
            ];
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "medialink-inner" => {
            let mut v = vec![
                Rule::rx_next(r"\]\]", "p", Next::Pop),
                Rule::groups(
                    r"(\|)([^\n=|]*)(=)",
                    &[
                        GroupAction::Tok("p", 1),
                        GroupAction::Tok("na", 2),
                        GroupAction::Tok("o", 3),
                    ],
                    Next::Stay,
                ),
                Rule::rx(r"\|", "p"),
            ];
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "extlink-inner" => {
            let mut v = vec![Rule::rx_next(r"\]", "p", Next::Pop)];
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "quote-common" => vec![
            Rule::rx_next(r"", "p", Next::Pop).guard(Guard::AtEnd(r"(?:\]\]|\{\{|\}\})")),
            Rule::rx_next(r"\n", "", Next::Pop),
        ],
        "inline-italic" => {
            let mut v = vec![
                Rule::groups(
                    r"('')(''')",
                    &[GroupAction::Tok("ge", 1), GroupAction::Tok("gs", 2)],
                    Next::PopPush("inline-bold"),
                )
                .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"'''", "ges", Next::PopPush("inline-italic-bold"))
                    .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"''", "ge", Next::Pop).guard(Guard::NotAtEnd("'")),
            ];
            include(&mut v, "quote-common");
            include(&mut v, "inline");
            v.extend(text_rules("ge"));
            v
        }
        "inline-bold" => {
            let mut v = vec![
                Rule::groups(
                    r"(''')('')",
                    &[GroupAction::Tok("gs", 1), GroupAction::Tok("ge", 2)],
                    Next::PopPush("inline-italic"),
                )
                .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"'''", "gs", Next::Pop).guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"''", "ges", Next::PopPush("inline-bold-italic"))
                    .guard(Guard::NotAtEnd("'")),
            ];
            include(&mut v, "quote-common");
            include(&mut v, "inline");
            v.extend(text_rules("gs"));
            v
        }
        "inline-bold-italic" => {
            let mut v = vec![
                Rule::groups(
                    r"('')(''')",
                    &[GroupAction::Tok("ges", 1), GroupAction::Tok("gs", 2)],
                    Next::Pop,
                )
                .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"'''", "ges", Next::PopPush("inline-italic"))
                    .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"''", "ges", Next::PopPush("inline-bold"))
                    .guard(Guard::NotAtEnd("'")),
            ];
            include(&mut v, "quote-common");
            include(&mut v, "inline");
            v.extend(text_rules("ges"));
            v
        }
        "inline-italic-bold" => {
            let mut v = vec![
                Rule::groups(
                    r"(''')('')",
                    &[GroupAction::Tok("ges", 1), GroupAction::Tok("ge", 2)],
                    Next::Pop,
                )
                .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"'''", "ges", Next::PopPush("inline-italic"))
                    .guard(Guard::NotAtEnd("'")),
                Rule::rx_next(r"''", "ges", Next::PopPush("inline-bold"))
                    .guard(Guard::NotAtEnd("'")),
            ];
            include(&mut v, "quote-common");
            include(&mut v, "inline");
            v.extend(text_rules("ges"));
            v
        }
        "text" => text_rules(""),
        "text-italic" => text_rules("ge"),
        "text-bold" => text_rules("gs"),
        "text-bold-italic" => text_rules("ges"),
        "entity" => vec![Rule::rx(r"&\S*?;", "ni")],
        "dt" => {
            let mut v = vec![Rule::rx_next(r"\n", "", Next::Pop)];
            include(&mut v, "inline");
            v.push(Rule::rx_next(r":", "k", Next::Pop));
            include(&mut v, "text");
            v
        }
        "table" => {
            let mut v = vec![
                Rule {
                    matcher: Matcher::TableEnd,
                    guard: Guard::None,
                    action: Action::Groups(&[GroupAction::Tok("w", 1), GroupAction::Tok("p", 2)]),
                    next: Next::Pop,
                },
                Rule {
                    matcher: Matcher::TableRow,
                    guard: Guard::None,
                    action: Action::Groups(&[
                        GroupAction::Tok("w", 1),
                        GroupAction::Tok("p", 2),
                        GroupAction::Relex(3, ATTR),
                    ]),
                    next: Next::Stay,
                },
                Rule {
                    matcher: Matcher::TableCaption,
                    guard: Guard::None,
                    action: Action::Groups(&[
                        GroupAction::Tok("w", 1),
                        GroupAction::Tok("p", 2),
                        GroupAction::Relex(3, ATTR),
                        GroupAction::Tok("p", 4),
                        GroupAction::Tok("gh", 5),
                    ]),
                    next: Next::Stay,
                },
                Rule {
                    matcher: Matcher::TableCell,
                    guard: Guard::None,
                    action: Action::Groups(&[
                        GroupAction::Tok("p", 1),
                        GroupAction::Relex(2, ATTR),
                        GroupAction::Tok("p", 3),
                    ]),
                    next: Next::Stay,
                },
                Rule {
                    matcher: Matcher::TableHeader,
                    guard: Guard::None,
                    action: Action::Groups(&[
                        GroupAction::Tok("p", 1),
                        GroupAction::Relex(2, ATTR),
                        GroupAction::Tok("p", 3),
                    ]),
                    next: Next::Push("table-header"),
                },
            ];
            include(&mut v, "list");
            include(&mut v, "inline");
            include(&mut v, "text");
            v
        }
        "table-header" => {
            let mut v = vec![
                Rule::rx_next(r"\n", "", Next::Pop),
                Rule {
                    matcher: Matcher::TableHeaderSep,
                    guard: Guard::None,
                    action: Action::Groups(&[
                        GroupAction::Tok("p", 1),
                        GroupAction::Relex(2, ATTR),
                        GroupAction::Tok("p", 3),
                    ]),
                    next: Next::Stay,
                },
            ];
            v.extend(text_rules("gu"));
            v
        }
        "attr" => {
            let mut v = Vec::new();
            include(&mut v, "replaceable");
            v.push(Rule::rx(r"\s+", "w"));
            v.push(Rule::groups(
                r#"(=)(\s*)(")"#,
                &[
                    GroupAction::Tok("o", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("s2", 3),
                ],
                Next::Push("attr-val-2"),
            ));
            v.push(Rule::groups(
                r"(=)(\s*)(')",
                &[
                    GroupAction::Tok("o", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("s1", 3),
                ],
                Next::Push("attr-val-1"),
            ));
            v.push(Rule::groups(
                r"(=)(\s*)",
                &[GroupAction::Tok("o", 1), GroupAction::Tok("w", 2)],
                Next::Push("attr-val-0"),
            ));
            v.push(Rule::rx(r"[\w:-]+", "na"));
            v
        }
        "attr-val-0" => {
            let mut v = vec![Rule::rx_next(r"\s", "w", Next::Pop)];
            include(&mut v, "replaceable");
            v.extend(text_rules("s"));
            v
        }
        "attr-val-1" => {
            let mut v = vec![Rule::rx_next(r"'", "s1", Next::Pop)];
            include(&mut v, "replaceable");
            v.extend(text_rules("s1"));
            v
        }
        "attr-val-2" => {
            let mut v = vec![Rule::rx_next(r#"""#, "s2", Next::Pop)];
            include(&mut v, "replaceable");
            v.extend(text_rules("s2"));
            v
        }
        "tag-inner-ordinary" => {
            let mut v = vec![Rule::rx_next(r"/?\s*>", "p", Next::Pop)];
            include(&mut v, "tag-attr");
            v
        }
        "tag-inner" => {
            let mut v = vec![
                Rule::rx_next(r"/\s*>", "p", Next::PopN(2)),
                Rule::rx_next(r"\s*>", "p", Next::Pop),
            ];
            include(&mut v, "tag-attr");
            v
        }
        "tag-attr" => {
            let mut v = Vec::new();
            include(&mut v, "replaceable");
            v.push(Rule::rx(r"\s+", "w"));
            v.push(Rule::groups(
                r#"(=)(\s*)(")"#,
                &[
                    GroupAction::Tok("o", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("s2", 3),
                ],
                Next::Push("tag-attr-val-2"),
            ));
            v.push(Rule::groups(
                r"(=)(\s*)(')",
                &[
                    GroupAction::Tok("o", 1),
                    GroupAction::Tok("w", 2),
                    GroupAction::Tok("s1", 3),
                ],
                Next::Push("tag-attr-val-1"),
            ));
            v.push(Rule::groups(
                r"(=)(\s*)",
                &[GroupAction::Tok("o", 1), GroupAction::Tok("w", 2)],
                Next::Push("tag-attr-val-0"),
            ));
            v.push(Rule::rx(r"[\w:-]+", "na"));
            v
        }
        "tag-attr-val-0" => {
            let mut v = vec![
                Rule::rx_next(r"\s", "w", Next::Pop),
                Rule::rx_next(r"/?>", "p", Next::PopN(2)),
            ];
            include(&mut v, "replaceable");
            v.extend(text_rules("s"));
            v
        }
        "tag-attr-val-1" => {
            let mut v = vec![
                Rule::rx_next(r"'", "s1", Next::Pop),
                Rule::rx_next(r"/?>", "p", Next::PopN(2)),
            ];
            include(&mut v, "replaceable");
            v.extend(text_rules("s1"));
            v
        }
        "tag-attr-val-2" => {
            let mut v = vec![
                Rule::rx_next(r#"""#, "s2", Next::Pop),
                Rule::rx_next(r"/?>", "p", Next::PopN(2)),
            ];
            include(&mut v, "replaceable");
            v.extend(text_rules("s2"));
            v
        }
        "tag-nowiki" => nowiki_tag(r"(?i)(</)(nowiki)(\s*)(>)"),
        "tag-pre" => nowiki_tag(r"(?i)(</)(pre)(\s*)(>)"),
        "tag-categorytree" => plaintext_tag(r"(?si)(.*?)(</)(categorytree)(\s*)(>)"),
        "tag-dynamicpagelist" => plaintext_tag(r"(?si)(.*?)(</)(dynamicpagelist)(\s*)(>)"),
        "tag-hiero" => plaintext_tag(r"(?si)(.*?)(</)(hiero)(\s*)(>)"),
        "tag-inputbox" => plaintext_tag(r"(?si)(.*?)(</)(inputbox)(\s*)(>)"),
        "tag-imagemap" => plaintext_tag(r"(?si)(.*?)(</)(imagemap)(\s*)(>)"),
        "tag-charinsert" => plaintext_tag(r"(?si)(.*?)(</)(charinsert)(\s*)(>)"),
        "tag-timeline" => plaintext_tag(r"(?si)(.*?)(</)(timeline)(\s*)(>)"),
        "tag-gallery" => plaintext_tag(r"(?si)(.*?)(</)(gallery)(\s*)(>)"),
        "tag-graph" => plaintext_tag(r"(?si)(.*?)(</)(graph)(\s*)(>)"),
        "tag-rss" => plaintext_tag(r"(?si)(.*?)(</)(rss)(\s*)(>)"),
        _ => Vec::new(),
    }
}

/// Pygments' `text_rules(token)`: words, then runs of blanks, then anything.
fn text_rules(class: &'static str) -> Vec<Rule> {
    vec![
        Rule::rx(r"\w+", class),
        Rule::rx(r"[^\S\n]+", class),
        Rule::rx(r"(?s).", class),
    ]
}

/// `nowiki_tag_rules(tag)`: the closing tag, else entities and text.
fn nowiki_tag(close: &'static str) -> Vec<Rule> {
    let mut v = vec![
        Rule::groups(
            close,
            &[
                GroupAction::Tok("p", 1),
                GroupAction::Tok("nt", 2),
                GroupAction::Tok("w", 3),
                GroupAction::Tok("p", 4),
            ],
            Next::Pop,
        ),
        Rule::rx(r"&\S*?;", "ni"),
    ];
    include(&mut v, "text");
    v
}

/// `plaintext_tag_rules(tag)`: everything up to the closing tag is text.
fn plaintext_tag(pattern: &'static str) -> Vec<Rule> {
    vec![Rule::groups(
        pattern,
        &[
            GroupAction::Tok("", 1),
            GroupAction::Tok("p", 2),
            GroupAction::Tok("nt", 3),
            GroupAction::Tok("w", 4),
            GroupAction::Tok("p", 5),
        ],
        Next::Pop,
    )]
}

/// Lex `text` as wikitext.
pub fn lex(text: &str) -> Vec<(&'static str, &str)> {
    super::lexer::lex(text, &["root"], compiled())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_compiles() {
        // `compile` drops a pattern that does not compile; a typo in the table
        // would otherwise show up as silently unhighlighted input.
        for name in STATE_NAMES {
            for rule in rules(name) {
                if let Matcher::Re(pattern) = rule.matcher {
                    assert!(
                        regex::Regex::new(&format!("(?m){pattern}")).is_ok(),
                        "state {name}: {pattern}"
                    );
                }
            }
        }
    }

    fn classes(text: &str) -> Vec<(&'static str, &str)> {
        lex(text)
            .into_iter()
            .filter(|(class, piece)| !class.is_empty() && !piece.is_empty())
            .collect()
    }

    #[test]
    fn a_template_call_is_lexed_like_pygments() {
        assert_eq!(
            classes("{{Infobox\n| label = x\n}}"),
            vec![
                ("p", "{{"),
                ("nt", "Infobox"),
                ("p", "|"),
                ("nl", " label "),
                ("o", "="),
                ("p", "}}"),
            ]
        );
    }

    #[test]
    fn a_parameter_keeps_its_name() {
        assert_eq!(
            classes("{{{name|x}}}"),
            vec![("p", "{{{"), ("nv", "name"), ("p", "|"), ("p", "}}}")]
        );
    }

    #[test]
    fn a_parser_function_is_a_name_function() {
        assert_eq!(
            classes("{{ #if: a | b }}"),
            vec![
                ("p", "{{"),
                ("w", " "),
                ("nf", "#if"),
                ("p", ":"),
                ("p", "|"),
                ("p", "}}"),
            ]
        );
    }

    #[test]
    fn a_table_lexes_its_cells_and_options() {
        // Verified against the service; a table outside the `table` state is
        // ordinary text, exactly as Pygments leaves it.
        assert!(classes("! a !! b\n").is_empty());
        assert_eq!(
            classes("{| class=\"wikitable\"\n|-\n! a !! b\n|}\n"),
            vec![
                ("p", "{|"),
                ("w", " "),
                ("na", "class"),
                ("o", "="),
                ("s2", "\""),
                ("s2", "wikitable"),
                ("s2", "\""),
                ("p", "|-"),
                ("p", "!"),
                ("gu", " "),
                ("gu", "a"),
                ("gu", " "),
                ("p", "!!"),
                ("gu", " "),
                ("gu", "b"),
                ("p", "|}"),
            ]
        );
    }

    #[test]
    fn a_media_link_lexes_its_options() {
        assert_eq!(
            classes("[[File:Example.jpg|200px|alt=Text]]"),
            vec![
                ("p", "[["),
                ("nn", "File"),
                ("p", ":"),
                ("nt", "Example.jpg"),
                ("p", "|"),
                ("p", "|"),
                ("na", "alt"),
                ("o", "="),
                ("p", "]]"),
            ]
        );
    }
}
