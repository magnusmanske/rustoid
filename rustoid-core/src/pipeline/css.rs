//! A faithful port of the pieces of `wikimedia/css-sanitizer` that decide the
//! *bytes* of a rendered stylesheet.
//!
//! TemplateStyles parses a stylesheet with `Wikimedia\CSS\Parser`, sanitises it,
//! and then serialises it with `Util::stringify($sheet, ['minify' => true])`.
//! The minified form is what reaches the page, so reproducing it byte-for-byte
//! means reproducing three things from that package:
//!
//! - the **tokeniser** (`Parser/DataSourceTokenizer.php`): CSS Syntax Level 3,
//!   which is where `'…'` strings become values, `url(…)` becomes a `url` token,
//!   and whitespace becomes a token of its own;
//! - `Token::__toString`: string/url re-quoting and escaping, ident escaping,
//!   numeric representations;
//! - `Util::stringify(…, minify)`: drop insignificant tokens, insert a
//!   `/**/` between two significant tokens that would otherwise merge, and keep
//!   a whitespace token only where `Token::separate` says the neighbours need it.
//!
//! The "significance" of a whitespace token comes from the grammar's
//! `mark-significance` match, which the sanitiser passes to `matchAgainst`. Two
//! cases matter here and they are handled by the callers, not guessed:
//! descendant-combinator whitespace in a selector (significant), and the
//! whitespace around `+`/`-` inside `calc()` (significant). Everywhere else a
//! whitespace token is insignificant and survives only where `separate` needs it.

/// A CSS token, mirroring `Wikimedia\CSS\Objects\Token`.
///
/// Numeric tokens keep their source representation rather than a parsed value:
/// `Token::__toString` writes the representation whenever it agrees numerically
/// with the value, which it always does for a token straight from the tokeniser.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Function(String),
    AtKeyword(String),
    /// `#name`; the flag is `true` for the `id` type flag (a valid ident name).
    Hash(String, bool),
    Str(String),
    Url(String),
    BadUrl,
    Delim(char),
    Number(String),
    Percentage(String),
    Dimension(String, String),
    Whitespace,
    Cdo,
    Cdc,
    Colon,
    Semicolon,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    LBrace,
    RBrace,
}

impl Token {
    /// `Token::__toString`.
    fn to_css(&self) -> String {
        match self {
            Token::Ident(s) => escape_ident(s),
            Token::Function(s) => format!("{}(", escape_ident(s)),
            Token::AtKeyword(s) => format!("@{}", escape_ident(s)),
            Token::Hash(s, true) => format!("#{}", escape_ident(s)),
            Token::Hash(s, false) => format!("#{}", escape_hash(s)),
            Token::Str(s) => format!("\"{}\"", escape_string(s)),
            Token::Url(s) => format!("url(\"{}\")", escape_string(s)),
            Token::BadUrl => "url(badurl'')".to_string(),
            Token::Delim('\\') => "\\\n".to_string(),
            Token::Delim(c) => c.to_string(),
            Token::Number(r) => r.clone(),
            Token::Percentage(r) => format!("{r}%"),
            Token::Dimension(r, unit) => {
                let unit = escape_ident(unit);
                if !r.contains('e') && !r.contains('E') && starts_exponential_notation(&unit) {
                    // A unit like `e5` would read as exponent notation, so the
                    // leading `e` is hex-escaped (`Token::__toString`).
                    let mut chars = unit.chars();
                    let first = chars.next().unwrap_or('e');
                    format!("{r}\\{:x} {}", first as u32, chars.as_str())
                } else {
                    format!("{r}{unit}")
                }
            }
            Token::Whitespace => " ".to_string(),
            Token::Cdo => "<!--".to_string(),
            Token::Cdc => "-->".to_string(),
            Token::Colon => ":".to_string(),
            Token::Semicolon => ";".to_string(),
            Token::Comma => ",".to_string(),
            Token::LBracket => "[".to_string(),
            Token::RBracket => "]".to_string(),
            Token::LParen => "(".to_string(),
            Token::RParen => ")".to_string(),
            Token::LBrace => "{".to_string(),
            Token::RBrace => "}".to_string(),
        }
    }

    /// The key `Token::separate` uses: the type name, or the delim character.
    fn key(&self) -> String {
        match self {
            Token::Ident(_) => "ident".into(),
            Token::Function(_) => "function".into(),
            Token::AtKeyword(_) => "at-keyword".into(),
            Token::Hash(..) => "hash".into(),
            Token::Str(_) => "string".into(),
            Token::Url(_) => "url".into(),
            Token::BadUrl => "bad-url".into(),
            Token::Delim(c) => c.to_string(),
            Token::Number(_) => "number".into(),
            Token::Percentage(_) => "percentage".into(),
            Token::Dimension(..) => "dimension".into(),
            Token::Whitespace => "whitespace".into(),
            Token::Cdo => "CDO".into(),
            Token::Cdc => "CDC".into(),
            Token::Colon => "colon".into(),
            Token::Semicolon => "semicolon".into(),
            Token::Comma => "comma".into(),
            Token::LBracket => "[".into(),
            Token::RBracket => "]".into(),
            Token::LParen => "(".into(),
            Token::RParen => ")".into(),
            Token::LBrace => "{".into(),
            Token::RBrace => "}".into(),
        }
    }
}

/// `Token::separate` — whether writing `first` and `second` next to each other
/// would change how they tokenise, so a `/**/` must go between them.
fn separate(first: &Token, second: &Token) -> bool {
    let t2 = second.key();
    let columns: &[&str] = match first.key().as_str() {
        "ident" => &[
            "ident",
            "function",
            "url",
            "bad-url",
            "-",
            "number",
            "percentage",
            "dimension",
            "CDC",
            "(",
            "hash",
        ],
        "at-keyword" => &[
            "ident",
            "function",
            "url",
            "bad-url",
            "-",
            "number",
            "percentage",
            "dimension",
            "CDC",
        ],
        "hash" | "dimension" => &[
            "ident",
            "function",
            "url",
            "bad-url",
            "-",
            "number",
            "percentage",
            "dimension",
            "CDC",
            "hash",
        ],
        "#" | "-" => &[
            "ident",
            "function",
            "url",
            "bad-url",
            "-",
            "number",
            "percentage",
            "dimension",
        ],
        "number" => &[
            "ident",
            "function",
            "url",
            "bad-url",
            "number",
            "percentage",
            "dimension",
            "%",
            "hash",
        ],
        "@" => &["ident", "function", "url", "bad-url", "-"],
        "." | "+" => &["number", "percentage", "dimension"],
        "/" => &["*"],
        "<" => &["ident", "function", "url", "bad-url", "!", "/"],
        _ => &[],
    };
    columns.contains(&t2.as_str())
}

/// Whether an escaped unit would read as exponent notation (`Token::__toString`).
fn starts_exponential_notation(unit: &str) -> bool {
    let mut chars = unit.chars();
    match chars.next() {
        Some('e' | 'E') => match chars.next() {
            Some(c) if c.is_ascii_digit() => true,
            Some('+' | '-') => chars.next().is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        },
        _ => false,
    }
}

/// Escape an identifier (`Token::escapeIdent`).
fn escape_ident(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        // Allowed unescaped: ASCII alphanumerics, `_`, `-`, and everything from
        // U+0080 up (`[\x{80}-\x{10ffff}]` in the source pattern).
        let allowed = c.is_ascii_alphanumeric() || c == '_' || c == '-' || (c as u32) >= 0x80;
        // A digit may not start the ident, nor follow a single leading `-`.
        let digit_at_start =
            c.is_ascii_digit() && (i == 0 || (i == 1 && chars.first() == Some(&'-')));
        if !allowed || digit_at_start {
            out.push_str(&escape_css_char(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Escape a non-`id` hash value (`Token::__toString`'s unrestricted branch).
fn escape_hash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let allowed = c.is_ascii_alphanumeric() || c == '_' || c == '-' || (c as u32) >= 0x80;
        if allowed {
            out.push(c);
        } else {
            out.push_str(&escape_css_char(c));
        }
    }
    out
}

/// Escape a string's contents (`Token::escapeString`).
///
/// Whitespace other than a plain space, controls, `"`, `\`, `<` and `>` need
/// escaping; everything else is written literally.
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if needs_string_escape(c) {
            out.push_str(&escape_css_char(c));
        } else {
            out.push(c);
        }
    }
    out
}

fn needs_string_escape(c: char) -> bool {
    (c.is_whitespace() && c != ' ') || is_css_control(c) || matches!(c, '"' | '\\' | '<' | '>')
}

/// Escape one character the way `Token::escapePregCallback` does.
///
/// Whitespace (other than a plain space), controls, hex digits and angle
/// brackets are written as a hex escape with a terminating space so they cannot
/// re-parse as part of a following token; anything else is simply backslashed.
fn escape_css_char(c: char) -> String {
    let code = c as u32;
    let hex_needed = (c.is_whitespace() && c != ' ')
        || is_css_control(c)
        || c.is_ascii_hexdigit()
        || matches!(c, '<' | '>');
    if hex_needed {
        format!("\\{code:x} ")
    } else {
        format!("\\{c}")
    }
}

/// The `[\p{Cc}\p{Cf}\p{Co}\p{Cs}]` set, approximated.
///
/// `char` in Rust cannot be a surrogate, so `Cs` cannot occur; the format and
/// private-use ranges are listed because they are the only realistic members
/// beyond `Cc` that a stylesheet could contain.
fn is_css_control(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e000}'..='\u{f8ff}'
            | '\u{f0000}'..='\u{ffffd}'
            | '\u{100000}'..='\u{10fffd}')
}

// ---------------------------------------------------------------------------
// Tokeniser
// ---------------------------------------------------------------------------

fn is_css_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (c as u32) >= 0x80
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-'
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn new(input: &str) -> Self {
        Lexer {
            chars: input.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.chars.get(self.pos + k).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Whether the input at the cursor starts a valid escape (`\` not before a
    /// newline).
    fn valid_escape(&self) -> bool {
        self.peek() == Some('\\') && !self.peek_at(1).is_some_and(is_newline)
    }

    /// Whether the input at the cursor starts an identifier.
    fn starts_ident(&self) -> bool {
        match self.peek() {
            Some('-') => match self.peek_at(1) {
                Some(c) if is_name_start(c) || c == '-' => true,
                Some('\\') => !self.peek_at(2).is_some_and(is_newline),
                _ => false,
            },
            Some('\\') => self.valid_escape(),
            Some(c) => is_name_start(c),
            None => false,
        }
    }

    /// Whether the input at the cursor starts a number (CSS Syntax's three-char
    /// lookahead).
    fn starts_number(&self) -> bool {
        match self.peek() {
            Some('+' | '-') => match self.peek_at(1) {
                Some(c) if c.is_ascii_digit() => true,
                Some('.') => self.peek_at(2).is_some_and(|c| c.is_ascii_digit()),
                _ => false,
            },
            Some('.') => self.peek_at(1).is_some_and(|c| c.is_ascii_digit()),
            Some(c) => c.is_ascii_digit(),
            None => false,
        }
    }

    /// Consume an escape sequence (the cursor is on the `\`).
    fn consume_escape(&mut self) -> char {
        self.bump();
        match self.peek() {
            None => '\u{fffd}',
            Some(c) if c.is_ascii_hexdigit() => {
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 6 {
                    match self.peek() {
                        Some(h) if h.is_ascii_hexdigit() => {
                            value = value * 16 + h.to_digit(16).unwrap_or(0);
                            self.bump();
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                // One whitespace character terminates a hex escape.
                if self.peek().is_some_and(is_css_whitespace) {
                    self.bump();
                }
                char::from_u32(value).unwrap_or('\u{fffd}')
            }
            Some(c) if is_newline(c) => '\u{fffd}',
            Some(c) => {
                self.bump();
                c
            }
        }
    }

    /// Consume a name (identifiers, function names, hash values, units).
    fn consume_name(&mut self) -> String {
        let mut out = String::new();
        loop {
            match self.peek() {
                Some(c) if is_name_char(c) => {
                    out.push(c);
                    self.bump();
                }
                Some('\\') if self.valid_escape() => out.push(self.consume_escape()),
                Some('\0') => {
                    out.push('\u{fffd}');
                    self.bump();
                }
                _ => break,
            }
        }
        out
    }

    /// Consume a numeric token's representation.
    fn consume_number(&mut self) -> String {
        let mut out = String::new();
        if matches!(self.peek(), Some('+' | '-')) {
            out.push(self.bump().unwrap_or('+'));
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            out.push('.');
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    out.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let exponent = match self.peek_at(1) {
                Some(c) if c.is_ascii_digit() => true,
                Some('+' | '-') => self.peek_at(2).is_some_and(|c| c.is_ascii_digit()),
                _ => false,
            };
            if exponent {
                out.push(self.bump().unwrap_or('e'));
                if matches!(self.peek(), Some('+' | '-')) {
                    out.push(self.bump().unwrap_or('+'));
                }
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        out.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
        }
        out
    }

    /// Consume a string; the cursor is on the opening quote.
    fn consume_string(&mut self, quote: char) -> Token {
        self.bump();
        let mut out = String::new();
        loop {
            match self.peek() {
                None => break,
                Some(c) if c == quote => {
                    self.bump();
                    break;
                }
                Some(c) if is_newline(c) => break,
                Some('\\') => {
                    if self.peek_at(1).is_some_and(is_newline) {
                        // An escaped newline is a line continuation: both go.
                        self.bump();
                        self.bump();
                    } else if self.peek_at(1).is_none() {
                        self.bump();
                    } else {
                        out.push(self.consume_escape());
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
        Token::Str(out)
    }

    /// Consume an ident-like token (ident, function, or url).
    fn consume_ident_like(&mut self) -> Token {
        let name = self.consume_name();
        if self.peek() != Some('(') {
            return Token::Ident(name);
        }
        self.bump();
        if name.eq_ignore_ascii_case("url") {
            // `url(` with a quoted argument is a plain function; otherwise the
            // contents are an unquoted url token.
            let mut look = self.pos;
            while self.chars.get(look).copied().is_some_and(is_css_whitespace) {
                look += 1;
            }
            match self.chars.get(look).copied() {
                Some('"' | '\'') => {
                    self.pos = look;
                    Token::Function(name)
                }
                _ => self.consume_unquoted_url(),
            }
        } else {
            Token::Function(name)
        }
    }

    /// Consume the body of an unquoted `url(…)`; the cursor is just past `(`.
    fn consume_unquoted_url(&mut self) -> Token {
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Token::Url(out),
                Some(')') => {
                    self.bump();
                    return Token::Url(out);
                }
                Some(c) if is_css_whitespace(c) => {
                    while self.peek().is_some_and(is_css_whitespace) {
                        self.bump();
                    }
                    return if self.peek() == Some(')') {
                        self.bump();
                        Token::Url(out)
                    } else {
                        self.consume_bad_url()
                    };
                }
                Some('"' | '\'' | '(') => return self.consume_bad_url(),
                Some('\\') if self.valid_escape() => out.push(self.consume_escape()),
                Some('\\') => return self.consume_bad_url(),
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
    }

    fn consume_bad_url(&mut self) -> Token {
        loop {
            match self.peek() {
                None => return Token::BadUrl,
                Some(')') => {
                    self.bump();
                    return Token::BadUrl;
                }
                Some('\\') if self.valid_escape() => {
                    self.consume_escape();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Consume one token.
    fn next_token(&mut self) -> Option<Token> {
        let c = self.peek()?;
        match c {
            c if is_css_whitespace(c) => {
                while self.peek().is_some_and(is_css_whitespace) {
                    self.bump();
                }
                Some(Token::Whitespace)
            }
            '"' | '\'' => Some(self.consume_string(c)),
            '#' => {
                self.bump();
                if self.peek().is_some_and(is_name_char) || self.valid_escape() {
                    let is_id = self.starts_ident();
                    let name = self.consume_name();
                    Some(Token::Hash(name, is_id))
                } else {
                    Some(Token::Delim('#'))
                }
            }
            '(' => {
                self.bump();
                Some(Token::LParen)
            }
            ')' => {
                self.bump();
                Some(Token::RParen)
            }
            '[' => {
                self.bump();
                Some(Token::LBracket)
            }
            ']' => {
                self.bump();
                Some(Token::RBracket)
            }
            '{' => {
                self.bump();
                Some(Token::LBrace)
            }
            '}' => {
                self.bump();
                Some(Token::RBrace)
            }
            ',' => {
                self.bump();
                Some(Token::Comma)
            }
            ':' => {
                self.bump();
                Some(Token::Colon)
            }
            ';' => {
                self.bump();
                Some(Token::Semicolon)
            }
            '+' => {
                if self.starts_number() {
                    Some(self.consume_numeric())
                } else {
                    self.bump();
                    Some(Token::Delim('+'))
                }
            }
            '-' => {
                if self.starts_number() {
                    Some(self.consume_numeric())
                } else if self.chars[self.pos..].starts_with(&['-', '-', '>']) {
                    self.bump();
                    self.bump();
                    self.bump();
                    Some(Token::Cdc)
                } else if self.starts_ident() {
                    Some(self.consume_ident_like())
                } else {
                    self.bump();
                    Some(Token::Delim('-'))
                }
            }
            '.' => {
                if self.starts_number() {
                    Some(self.consume_numeric())
                } else {
                    self.bump();
                    Some(Token::Delim('.'))
                }
            }
            '<' => {
                if self.chars[self.pos..].starts_with(&['<', '!', '-', '-']) {
                    for _ in 0..4 {
                        self.bump();
                    }
                    Some(Token::Cdo)
                } else {
                    self.bump();
                    Some(Token::Delim('<'))
                }
            }
            '@' => {
                self.bump();
                if self.starts_ident() {
                    Some(Token::AtKeyword(self.consume_name()))
                } else {
                    Some(Token::Delim('@'))
                }
            }
            '\\' => {
                if self.valid_escape() {
                    Some(self.consume_ident_like())
                } else {
                    self.bump();
                    Some(Token::Delim('\\'))
                }
            }
            c if c.is_ascii_digit() => Some(self.consume_numeric()),
            c if is_name_start(c) => Some(self.consume_ident_like()),
            _ => {
                self.bump();
                Some(Token::Delim(c))
            }
        }
    }

    fn consume_numeric(&mut self) -> Token {
        let repr = self.consume_number();
        if self.peek() == Some('%') {
            self.bump();
            Token::Percentage(repr)
        } else if self.starts_ident() {
            Token::Dimension(repr, self.consume_name())
        } else {
            Token::Number(repr)
        }
    }
}

fn is_newline(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\x0c')
}

/// Tokenise a CSS fragment, discarding the EOF token.
fn tokenize(input: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(input);
    let mut out = Vec::new();
    while let Some(t) = lexer.next_token() {
        out.push(t);
    }
    out
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

/// `Util::stringify($tokens, ['minify' => true])`.
///
/// `forced` marks whitespace tokens that the grammar matched as significant
/// (descendant combinators, `calc()` operators); the rest are kept only where
/// `separate` needs them.
fn stringify_tokens(tokens: &[Token], forced: &[bool]) -> String {
    let n = tokens.len();
    let mut significant: Vec<bool> = (0..n)
        .map(|i| !matches!(tokens[i], Token::Whitespace) || forced.get(i) == Some(&true))
        .collect();
    for i in 1..n.saturating_sub(1) {
        if matches!(tokens[i], Token::Whitespace)
            && !significant[i]
            && separate(&tokens[i - 1], &tokens[i + 1])
        {
            significant[i] = true;
        }
    }
    let mut out = String::new();
    let mut prev: Option<&Token> = None;
    for (i, t) in tokens.iter().enumerate() {
        if !significant[i] {
            continue;
        }
        if let Some(p) = prev
            && separate(p, t)
        {
            out.push_str("/**/");
        }
        out.push_str(&t.to_css());
        prev = Some(t);
    }
    out
}

/// Serialise a declaration value or an at-rule prelude.
///
/// Every whitespace token is insignificant unless `calc()`-family whitespace
/// around a `+`/`-` operator, which the property grammar matches explicitly.
pub fn value(input: &str) -> String {
    let tokens = tokenize(input);
    let forced = calc_whitespace(&tokens);
    stringify_tokens(&tokens, &forced)
}

/// Mark the whitespace around `+`/`-` inside a `calc()`-family function as
/// significant (`calc`/`min`/`max`/`clamp` share `MatcherFactory`'s calc grammar).
fn calc_whitespace(tokens: &[Token]) -> Vec<bool> {
    let mut forced = vec![false; tokens.len()];
    let mut stack: Vec<bool> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Function(name) => {
                let in_calc = stack.last().copied().unwrap_or(false)
                    || matches!(
                        name.to_ascii_lowercase().as_str(),
                        "calc" | "min" | "max" | "clamp"
                    );
                stack.push(in_calc);
            }
            Token::LParen => stack.push(stack.last().copied().unwrap_or(false)),
            Token::RParen => {
                stack.pop();
            }
            Token::Whitespace if stack.last().copied().unwrap_or(false) => {
                let prev = prev_non_ws(tokens, i);
                let next = next_non_ws(tokens, i);
                if [prev, next]
                    .iter()
                    .any(|t| matches!(t, Some(Token::Delim('+' | '-'))))
                {
                    forced[i] = true;
                }
            }
            _ => {}
        }
    }
    forced
}

fn prev_non_ws(tokens: &[Token], i: usize) -> Option<&Token> {
    tokens[..i]
        .iter()
        .rev()
        .find(|t| !matches!(t, Token::Whitespace))
}

fn next_non_ws(tokens: &[Token], i: usize) -> Option<&Token> {
    tokens[i + 1..]
        .iter()
        .find(|t| !matches!(t, Token::Whitespace))
}

// ---------------------------------------------------------------------------
// Selector scoping
// ---------------------------------------------------------------------------

/// Tokenise a selector into top-level items: compounds separated by a
/// descendant combinator (significant whitespace) or a `>`/`+`/`~` combinator.
enum Item {
    Compound(usize, usize),
    Ws(usize),
    Comb,
}

/// Split a selector's tokens into `Item`s at paren-depth 0.
///
/// A whitespace token that is the descendant combinator becomes `Ws`; whitespace
/// next to a `>`/`+`/`~` is part of that combinator's own spacing and is dropped.
fn selector_items(tokens: &[Token]) -> Vec<Item> {
    let is_combinator = |t: &Token| matches!(t, Token::Delim('>' | '+' | '~'));
    let mut items: Vec<Item> = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for i in 0..tokens.len() {
        let token = &tokens[i];
        if matches!(token, Token::Whitespace) && depth == 0 {
            // Whitespace next to a `>`/`+`/`~` belongs to that combinator;
            // a bare whitespace *is* the descendant combinator.
            let prev_comb = prev_non_ws(tokens, i).is_some_and(is_combinator);
            let next_comb = next_non_ws(tokens, i).is_some_and(is_combinator);
            if prev_comb || next_comb {
                continue;
            }
            if let Some(s) = start.take()
                && s < i
            {
                items.push(Item::Compound(s, i));
            }
            items.push(Item::Ws(i));
            continue;
        }
        if depth == 0 && matches!(token, Token::Delim('>' | '+' | '~')) {
            if let Some(s) = start.take()
                && s < i
            {
                items.push(Item::Compound(s, i));
            }
            items.push(Item::Comb);
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
        match token {
            Token::LParen | Token::Function(_) | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if let Some(s) = start {
        items.push(Item::Compound(s, tokens.len()));
    }
    items
}

/// `true` when a compound selector's first token is the `html` or `body` element.
fn is_html_or_body(compound: &[Token]) -> bool {
    matches!(compound.first(), Some(Token::Ident(e)) if e == "html" || e == "body")
}

/// The index of the descendant-combinator whitespace that separates the
/// hoistable prefix from the rest of the selector, if there is one.
///
/// Mirrors the `hoistableComponentMatcher` in TemplateStyles' `Hooks`: the
/// longest leading run of `html`/`body`-led compound selectors joined by
/// descendant combinators, followed by a descendant combinator and a postfix.
fn hoist_split(tokens: &[Token]) -> Option<usize> {
    let items = selector_items(tokens);
    let mut m = 0usize;
    let mut total = 0usize;
    for item in &items {
        if matches!(item, Item::Compound(..)) {
            total += 1;
        }
    }
    while let Some(Item::Compound(a, b)) = items.get(2 * m) {
        if !is_html_or_body(&tokens[*a..*b]) {
            break;
        }
        if m > 0 && !matches!(items.get(2 * m - 1), Some(Item::Ws(_))) {
            break;
        }
        m += 1;
    }
    if m == 0 || m >= total {
        return None;
    }
    match items.get(2 * m - 1) {
        Some(Item::Ws(w)) => Some(*w),
        _ => None,
    }
}

/// Scope each selector in `selector` to `scope`, following TemplateStyles'
/// `StyleRuleSanitizer` with a hoistable `html`/`body` prefix.
///
/// `scope` is `.mw-parser-output` (or `.mw-parser-output <wrapper>`), inserted
/// with a significant space on either side, exactly as the sanitizer's
/// `prependSelectors` are.
pub fn scope_selector(selector: &str, scope: &str) -> String {
    let scope_tokens = tokenize(scope);
    let scope_forced: Vec<bool> = scope_tokens
        .iter()
        .map(|t| matches!(t, Token::Whitespace))
        .collect();

    let mut tokens: Vec<Token> = Vec::new();
    let mut forced: Vec<bool> = Vec::new();
    for (i, part) in split_top_level_commas(selector).iter().enumerate() {
        if i > 0 {
            tokens.push(Token::Comma);
            forced.push(false);
        }
        let sel = tokenize(part);
        let sel_forced = descendant_whitespace(&sel);
        match hoist_split(&sel) {
            Some(ws) => {
                // prefix up to (not including) the separating whitespace
                push_range(&mut tokens, &mut forced, &sel, &sel_forced, 0, ws);
                tokens.push(Token::Whitespace);
                forced.push(true);
                push_range(
                    &mut tokens,
                    &mut forced,
                    &scope_tokens,
                    &scope_forced,
                    0,
                    scope_tokens.len(),
                );
                tokens.push(Token::Whitespace);
                forced.push(true);
                push_range(
                    &mut tokens,
                    &mut forced,
                    &sel,
                    &sel_forced,
                    ws + 1,
                    sel.len(),
                );
            }
            None => {
                push_range(
                    &mut tokens,
                    &mut forced,
                    &scope_tokens,
                    &scope_forced,
                    0,
                    scope_tokens.len(),
                );
                tokens.push(Token::Whitespace);
                forced.push(true);
                push_range(&mut tokens, &mut forced, &sel, &sel_forced, 0, sel.len());
            }
        }
    }
    stringify_tokens(&tokens, &forced)
}

#[allow(clippy::too_many_arguments)]
fn push_range(
    tokens: &mut Vec<Token>,
    forced: &mut Vec<bool>,
    src: &[Token],
    src_forced: &[bool],
    from: usize,
    to: usize,
) {
    tokens.extend(src[from..to].iter().cloned());
    forced.extend(src_forced[from..to].iter().copied());
}

/// The significance flags for a selector: whitespace that is the descendant
/// combinator is significant, whitespace around `>`/`+`/`~` is not.
fn descendant_whitespace(tokens: &[Token]) -> Vec<bool> {
    let mut forced = vec![false; tokens.len()];
    for item in selector_items(tokens) {
        if let Item::Ws(i) = item {
            forced[i] = true;
        }
    }
    forced
}

/// Split a selector string's tokens on top-level commas, returning substrings.
fn split_top_level_commas(selector: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in selector.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(selector[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(selector[start..].trim().to_string());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four string tokens of a `quotes` declaration are each re-serialised
    /// with double quotes and an escaped inner quote, with the whitespace
    /// between them dropped — exactly what the CS1 stylesheet serves.
    #[test]
    fn strings_are_requoted_and_escaped() {
        assert_eq!(
            value("\x27\"\x27 \x27\"\x27 \"\x27\" \"\x27\""),
            r#""\"""\"""'""'""#
        );
    }

    /// An unquoted `url(…)` becomes a quoted one, and a `url(…)` chained to the
    /// next component loses the separating whitespace.
    #[test]
    fn urls_are_quoted() {
        assert_eq!(
            value("url(//upload.example/a.svg)\n\t\tright 0.1em"),
            "url(\"//upload.example/a.svg\")right 0.1em"
        );
    }

    /// Whitespace inside `calc()` around an operator is significant; whitespace
    /// inside another function is not.
    #[test]
    fn calc_keeps_operator_whitespace() {
        assert_eq!(value("calc(100% - 0.7em)"), "calc(100% - 0.7em)");
        assert_eq!(value("var(--x, #fff)"), "var(--x,#fff)");
    }

    /// A `html body.x` prefix is hoisted before the scope; an ordinary selector
    /// is scoped at the front.
    #[test]
    fn hoistable_prefix_goes_before_the_scope() {
        assert_eq!(
            scope_selector("html body.mediawiki .ambox", ".mw-parser-output"),
            "html body.mediawiki .mw-parser-output .ambox"
        );
        assert_eq!(
            scope_selector("body.ns-0 .hatnote", ".mw-parser-output"),
            "body.ns-0 .mw-parser-output .hatnote"
        );
        assert_eq!(
            scope_selector(".hatnote + .mw-empty-elt", ".mw-parser-output"),
            ".mw-parser-output .hatnote+.mw-empty-elt"
        );
        // Whitespace inside `:not()` is insignificant, but the scope still
        // joins the selector with a significant space.
        assert_eq!(
            scope_selector(":not( .x )", ".mw-parser-output"),
            ".mw-parser-output :not(.x)"
        );
    }
}
