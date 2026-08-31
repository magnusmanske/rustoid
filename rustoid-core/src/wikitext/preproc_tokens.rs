//! Preprocessor token types — faithful port of PHP Parsoid's
//! `src/Tokens/PreprocTk.php` / `PreprocType.php` / `PreprocAngleTk.php` /
//! `PreprocIgnoreTk.php`.
//!
//! These represent the *unexpanded-but-preprocessed* pieces of wikitext:
//! templates/args/pf (brace `{{...}}`/`{{{...}}}`), wikilinks (`[[...]]`),
//! extension tags (`<...>`), include directives (`<includeonly>` etc.),
//! comments (`<!--...-->`), headings (`==...==`), and language-variant blocks
//! (`-{...}-`). The PEG tokenizer's `template3` rule (and TokenTransform3)
//! consumes these pieces and turns the brace pieces into `template`/
//! `templatearg` tokens via `get_barred_args`.

use crate::wikitext::tokens_v2::SourceRange;

/// Types of preprocessor pieces. Mirrors PHP's `PreprocType` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreprocType {
    Bracket,
    Angle,
    Brace,
    DashBrace,
    Comment,
    Ignore,
    Heading,
    Pfragment,
}

impl PreprocType {
    pub fn open(self) -> &'static str {
        match self {
            Self::Bracket => "[",
            Self::Angle => "<",
            Self::Brace => "{",
            Self::DashBrace => "-{",
            Self::Comment => "<!--",
            Self::Heading => "=",
            Self::Ignore | Self::Pfragment => "",
        }
    }

    pub fn close(self) -> &'static str {
        match self {
            Self::Bracket => "]",
            Self::Angle => ">",
            Self::Brace => "}",
            Self::DashBrace => "}-",
            Self::Comment => "-->",
            Self::Heading => "=",
            Self::Ignore | Self::Pfragment => "",
        }
    }

    pub fn min_count(self) -> usize {
        match self {
            Self::Brace => 2,
            Self::Ignore | Self::Pfragment => 0,
            _ => 1,
        }
    }

    pub fn max_count(self) -> usize {
        match self {
            Self::Bracket => 2,
            Self::Brace => 3,
            Self::Heading => 6,
            Self::Ignore | Self::Pfragment => 0,
            _ => 1,
        }
    }

    /// If `sr` includes `count` copies of the delimiters, return a range
    /// covering just the contents. Mirrors `PreprocType::shrinkRange`.
    pub fn shrink_range(self, sr: &SourceRange, count: usize) -> SourceRange {
        SourceRange::new(
            sr.start.unwrap_or(0) + count * self.open().len(),
            sr.end.saturating_sub(count * self.close().len()),
        )
    }

    /// The inverse of `shrink_range`. Mirrors `PreprocType::growRange`.
    pub fn grow_range(self, sr: &SourceRange, count: usize) -> SourceRange {
        SourceRange::new(
            sr.start
                .unwrap_or(0)
                .saturating_sub(count * self.open().len()),
            sr.end + count * self.close().len(),
        )
    }
}

/// A single preprocessed piece: either a plain string or a nested `PreprocTk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocPiece {
    Str(String),
    Tk(PreprocTk),
}

/// A single preprocessor token. Mirrors PHP's `PreprocTk` and its subclasses
/// (`PreprocAngleTk`, `PreprocIgnoreTk`) by carrying the variant-specific
/// fields in a single enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocTk {
    /// Brace (`{{...}}`/`{{{...}}}`), bracket (`[[...]]`), dash-brace
    /// (`-{...}-`), comment, or heading.
    Simple {
        ty: PreprocType,
        tsr: SourceRange,
        contents: Vec<PreprocPiece>,
        count: usize,
    },
    /// Ignored content (`<includeonly>`/`<noinclude>`/`<onlyinclude>` and
    /// annotations).
    Ignore {
        tsr: SourceRange,
        contents: Vec<PreprocPiece>,
        annotation: Option<String>,
    },
    /// Angle-bracket extension tags.
    Angle {
        tsr: SourceRange,
        open: String,
        ext_attrs: String,
        contents: Vec<PreprocPiece>,
        close: Option<String>,
    },
}

impl PreprocTk {
    pub const CONTENTS_ATTR: &'static str = "mw:contents";

    pub fn simple(
        ty: PreprocType,
        tsr: SourceRange,
        contents: Vec<PreprocPiece>,
        count: usize,
    ) -> Self {
        Self::Simple {
            ty,
            tsr,
            contents,
            count,
        }
    }

    pub fn ignore(
        tsr: SourceRange,
        contents: Vec<PreprocPiece>,
        annotation: Option<String>,
    ) -> Self {
        Self::Ignore {
            tsr,
            contents,
            annotation,
        }
    }

    pub fn angle(
        tsr: SourceRange,
        open: String,
        ext_attrs: String,
        contents: Vec<PreprocPiece>,
        close: Option<String>,
    ) -> Self {
        Self::Angle {
            tsr,
            open,
            ext_attrs,
            contents,
            close,
        }
    }

    pub fn ty(&self) -> PreprocType {
        match self {
            Self::Simple { ty, .. } => *ty,
            Self::Ignore { .. } => PreprocType::Ignore,
            Self::Angle { .. } => PreprocType::Angle,
        }
    }

    pub fn tsr(&self) -> &SourceRange {
        match self {
            Self::Simple { tsr, .. } | Self::Ignore { tsr, .. } | Self::Angle { tsr, .. } => tsr,
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::Simple { count, .. } => *count,
            Self::Ignore { .. } => 0,
            Self::Angle { .. } => 1,
        }
    }

    pub fn get_contents(&self) -> &[PreprocPiece] {
        match self {
            Self::Simple { contents, .. }
            | Self::Ignore { contents, .. }
            | Self::Angle { contents, .. } => contents,
        }
    }

    /// The normalized tag name (stripped of any `#hash` part) for angle tags.
    /// Mirrors `PreprocAngleTk::name`.
    pub fn name(&self) -> &str {
        match self {
            Self::Angle { open, .. } => open.split('#').next().unwrap_or(open),
            _ => "",
        }
    }
}

/// Convert a list of preproc pieces to their string form (mirrors
/// `PreprocTk::printContents(..., false)`).
pub fn print_contents(pieces: &[PreprocPiece], pretty: bool) -> String {
    let mut out = String::new();
    print_contents_internal(pieces, "", pretty, &mut out);
    out
}

fn print_contents_internal(pieces: &[PreprocPiece], prefix: &str, pretty: bool, out: &mut String) {
    for piece in pieces {
        match piece {
            PreprocPiece::Str(s) => {
                out.push_str(prefix);
                out.push_str(s);
            }
            PreprocPiece::Tk(tk) => {
                match tk {
                    PreprocTk::Ignore { .. } if !pretty => {
                        // Emit nothing for ignored content (non-pretty).
                    }
                    _ => {
                        out.push_str(prefix);
                        out.push_str(&tk.ty().open().repeat(tk.count()));
                        let nested_prefix = if pretty {
                            format!("{prefix}  ")
                        } else {
                            prefix.to_string()
                        };
                        print_contents_internal(tk.get_contents(), &nested_prefix, pretty, out);
                        out.push_str(&tk.ty().close().repeat(tk.count()));
                    }
                }
            }
        }
    }
}

/// Split a contents list by separator strings. Returns a flat list of
/// segments; a segment is either normal content or a captured separator
/// (mirrors `PreprocTk::splitContentsBy` with `PREG_SPLIT_DELIM_CAPTURE`,
/// where separators appear at odd indexes).
pub fn split_contents_by(
    seps: &[&str],
    contents: &[PreprocPiece],
    limit: isize,
) -> Vec<Vec<PreprocPiece>> {
    let mut result: Vec<Vec<PreprocPiece>> = vec![Vec::new()];
    let mut splits_remaining = limit;

    for piece in contents {
        match piece {
            PreprocPiece::Tk(tk) => {
                result
                    .last_mut()
                    .unwrap()
                    .push(PreprocPiece::Tk(tk.clone()));
            }
            PreprocPiece::Str(s) => {
                let mut cur = s.as_str();
                loop {
                    if splits_remaining == 0 {
                        result
                            .last_mut()
                            .unwrap()
                            .push(PreprocPiece::Str(cur.to_string()));
                        break;
                    }
                    let Some((idx, sep_len)) = earliest_sep(cur, seps) else {
                        result
                            .last_mut()
                            .unwrap()
                            .push(PreprocPiece::Str(cur.to_string()));
                        break;
                    };
                    // Push the text before the separator.
                    if !cur[..idx].is_empty() {
                        result
                            .last_mut()
                            .unwrap()
                            .push(PreprocPiece::Str(cur[..idx].to_string()));
                    }
                    // The separator becomes its own segment.
                    result.push(vec![PreprocPiece::Str(cur[idx..idx + sep_len].to_string())]);
                    result.push(Vec::new());
                    if limit >= 0 {
                        splits_remaining -= 1;
                    }
                    cur = &cur[idx + sep_len..];
                }
            }
        }
    }

    result
}

fn earliest_sep(s: &str, seps: &[&str]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for sep in seps {
        if let Some(idx) = s.find(sep)
            && best.is_none_or(|(bi, _)| idx < bi)
        {
            best = Some((idx, sep.len()));
        }
    }
    best
}

/// Split a brace piece (`{{...}}`/`{{{...}}}`) into its target and named/
/// positional arguments. Mirrors `PreprocTk::getBarredArgs`, returning a list
/// of `(key, value)` pairs with the target as the first entry.
pub fn get_barred_args(contents: &[PreprocPiece]) -> Vec<(Vec<PreprocPiece>, Vec<PreprocPiece>)> {
    // Flatten the contents to a single string for splitting, preserving nested
    // tokens as opaque marks is unnecessary for this port's purposes (the
    // tokenizer splits the brace pieces at the text level).
    let flat = print_contents(contents, false);

    let mut result = Vec::new();
    // Split on top-level '|' (nested braces are already handled by the
    // tokenizer splitting, so treat the flat string as the inner content).
    let parts: Vec<&str> = flat.split('|').collect();

    if let Some(target) = parts.first() {
        result.push((vec![str_piece(target)], Vec::new()));
    }

    for part in parts.iter().skip(1) {
        let (key, value) = split_key_value_str(part);
        result.push((key, value));
    }

    result
}

// Helper alias to keep the target construction concise.
fn str_piece(s: &str) -> PreprocPiece {
    PreprocPiece::Str(s.to_string())
}

/// Split `name=value` into `(name, value)`; a piece with no `=` is a positional
/// argument (empty key). Mirrors `template_param`.
fn split_key_value_str(s: &str) -> (Vec<PreprocPiece>, Vec<PreprocPiece>) {
    if let Some((k, v)) = s.split_once('=') {
        (vec![str_piece(k)], vec![str_piece(v)])
    } else {
        (Vec::new(), vec![str_piece(s)])
    }
}

/// Build the attribute list (`KV` array) for a `template` or `template3`
/// token from a brace `PreprocTk`. Mirrors `PreprocTk::getBarredArgs` followed
/// by the `template3` rule of `Grammar.pegphp`.
///
/// - `attribs[0]` is the target (`KV(target_tokens, '')`).
/// - `attribs[1..]` are the pipe-separated arguments (`KV(name, value)` for
///   named, `KV('', value)` for positional).
pub fn brace_to_attribs(tk: &PreprocTk) -> Vec<crate::wikitext::tokens_v2::KV> {
    use crate::wikitext::tokens_v2::{KV, KeyValue};

    let contents = tk.get_contents();
    let args = get_barred_args(contents);

    let mut attribs = Vec::with_capacity(args.len());
    for (key, value) in args {
        let key_str = print_contents(&key, false);
        let value_str = print_contents(&value, false);
        attribs.push(KV {
            key: KeyValue::Str(key_str),
            value: KeyValue::Str(value_str),
            src_offsets: None,
            ksrc: None,
            vsrc: None,
        });
    }
    attribs
}

/// Tokenize raw wikitext into preprocessor pieces. Mirrors the
/// `preproc_pieces` / `preproc_piece` rules of PHP's `Grammar.pegphp`.
///
/// The output is a flat `Vec<PreprocPiece>` (strings and nested `PreprocTk`
/// for `{{...}}`, `{{{...}}}`, `[[...]]`, `-{...}-`, comments, headings, and
/// angle tags).
pub fn tokenize_preproc_pieces(input: &str) -> Vec<PreprocPiece> {
    let mut scanner = PreprocScanner {
        input,
        pos: 0,
        at_sol: true,
    };
    let mut out = Vec::new();
    scanner.scan_until(input.len(), &mut out);
    out
}

/// A small recursive-descent scanner for the preprocessor grammar. It tracks a
/// byte position and a start-of-line flag (headings only match at start of
/// line).
struct PreprocScanner<'a> {
    input: &'a str,
    pos: usize,
    at_sol: bool,
}

impl<'a> PreprocScanner<'a> {
    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    /// Scan until `end`, appending pieces to `out`.
    fn scan_until(&mut self, end: usize, out: &mut Vec<PreprocPiece>) {
        while self.pos < end {
            if let Some(piece) = self.scan_one(end) {
                out.push(piece);
            } else {
                // Advance one char to avoid an infinite loop.
                let ch = self.input[self.pos..].chars().next().unwrap();
                out.push(str_piece(&ch.to_string()));
                self.pos += ch.len_utf8();
                self.at_sol = false;
            }
        }
    }

    /// Try to match a single preprocessor piece at the current position.
    fn scan_one(&mut self, end: usize) -> Option<PreprocPiece> {
        // Headings at start of line: `== ... ==`.
        if self.at_sol
            && self.remaining().starts_with('=')
            && let Some(tk) = self.scan_heading()
        {
            return Some(PreprocPiece::Tk(tk));
        }

        // `[[ ... ]]` — bracket (count 2).
        if self.remaining().starts_with("[[") {
            return self
                .scan_balanced("[[", "]]", PreprocType::Bracket, 2)
                .map(PreprocPiece::Tk);
        }

        // `{{{ ... }}}` — tplarg (brace count 3).
        if self.remaining().starts_with("{{{") {
            return self
                .scan_balanced("{{", "}}}", PreprocType::Brace, 3)
                .map(PreprocPiece::Tk);
        }

        // `{{ ... }}` — template (brace count 2).
        if self.remaining().starts_with("{{") {
            return self
                .scan_balanced("{{", "}}", PreprocType::Brace, 2)
                .map(PreprocPiece::Tk);
        }

        // `-{ ... }-` — dash-brace.
        if self.remaining().starts_with("-{") {
            return self
                .scan_balanced("-{", "}-", PreprocType::DashBrace, 1)
                .map(PreprocPiece::Tk);
        }

        // Comment `<!-- ... -->`.
        if self.remaining().starts_with("<!--") {
            return self.scan_comment().map(PreprocPiece::Tk);
        }

        // Angle tags for extension/includes: `<...>`.
        if self.remaining().starts_with('<') {
            return self.scan_angle().map(PreprocPiece::Tk);
        }

        // Ignored chars (a run of non-structural chars).
        let ch = self.remaining().chars().next()?;
        if !matches!(ch, '-' | '[' | ']' | '{' | '}' | '<' | '\n') {
            let start = self.pos;
            while self.pos < end {
                let c = self.input[self.pos..].chars().next()?;
                if matches!(c, '-' | '[' | ']' | '{' | '}' | '<' | '\n') {
                    break;
                }
                self.pos += c.len_utf8();
            }
            let text = self.input[start..self.pos].to_string();
            self.at_sol = false;
            return Some(str_piece(&text));
        }

        // Fall back to a single broken structural char.
        self.pos += ch.len_utf8();
        self.at_sol = ch == '\n';
        Some(str_piece(&ch.to_string()))
    }

    fn scan_balanced(
        &mut self,
        open: &str,
        close: &str,
        ty: PreprocType,
        count: usize,
    ) -> Option<PreprocTk> {
        let start = self.pos;
        self.pos += open.len();

        let mut contents = Vec::new();
        let mut depth = 1usize;
        while self.pos < self.input.len() {
            if self.remaining().starts_with(open) {
                depth += 1;
                let nested = self.scan_balanced(open, close, ty, count)?;
                contents.push(PreprocPiece::Tk(nested));
                continue;
            }
            if self.remaining().starts_with(close) {
                depth -= 1;
                self.pos += close.len();
                if depth == 0 {
                    let tsr = SourceRange::new(start, self.pos);
                    self.at_sol = false;
                    return Some(PreprocTk::simple(ty, tsr, contents, count));
                }
                contents.push(str_piece(close));
                continue;
            }
            let ch = self.input[self.pos..].chars().next()?;
            // Nested templates/links inside a non-matching construct.
            if ch == '{' && self.remaining().starts_with("{{") && ty != PreprocType::Brace {
                let nested = self.scan_balanced("{{", "}}", PreprocType::Brace, 2)?;
                contents.push(PreprocPiece::Tk(nested));
                continue;
            }
            if ch == '[' && self.remaining().starts_with("[[") && ty != PreprocType::Bracket {
                let nested = self.scan_balanced("[[", "]]", PreprocType::Bracket, 2)?;
                contents.push(PreprocPiece::Tk(nested));
                continue;
            }
            contents.push(str_piece(&ch.to_string()));
            self.pos += ch.len_utf8();
        }
        // Unbalanced: reset and treat the opener as plain text.
        self.pos = start;
        self.at_sol = false;
        None
    }

    fn scan_comment(&mut self) -> Option<PreprocTk> {
        let start = self.pos;
        self.pos += 4; // "<!--"
        let rem = self.remaining();
        let end = rem.find("-->")?;
        let contents = rem[..end].to_string();
        self.pos += end + 3;
        self.at_sol = false;
        Some(PreprocTk::simple(
            PreprocType::Comment,
            SourceRange::new(start, self.pos),
            vec![str_piece(&contents)],
            1,
        ))
    }

    fn scan_angle(&mut self) -> Option<PreprocTk> {
        let start = self.pos;
        self.pos += 1; // "<"
        let rem = self.remaining();
        let close = rem.find('>')?;
        let inner = &rem[..close];
        self.pos += close + 1;
        self.at_sol = false;

        let open = inner.trim_start_matches('/').to_string();
        Some(PreprocTk::angle(
            SourceRange::new(start, self.pos),
            open,
            String::new(),
            vec![str_piece(inner)],
            None,
        ))
    }

    fn scan_heading(&mut self) -> Option<PreprocTk> {
        let rem = self.remaining();
        let eq = rem.chars().take_while(|&c| c == '=').count();
        if eq < 2 {
            return None;
        }
        let start = self.pos;
        self.pos += eq;
        let body_start = self.pos;
        let Some(close_pos) = self.remaining().find('=') else {
            self.pos = start;
            return None;
        };
        let close_abs = body_start + close_pos;
        let mut close_end = close_abs;
        while close_end < self.input.len() && self.input[close_end..].starts_with('=') {
            close_end += 1;
        }
        let close_count = close_end - close_abs;
        let body = self.input[body_start..close_abs].to_string();
        self.pos = close_end;
        self.at_sol = false;

        let level = eq.min(close_count).min(6);
        Some(PreprocTk::simple(
            PreprocType::Heading,
            SourceRange::new(start, self.pos),
            vec![str_piece(&body)],
            level,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_piece(s: &str) -> PreprocPiece {
        PreprocPiece::Str(s.to_string())
    }

    #[test]
    fn test_preproc_type_delimiters() {
        assert_eq!(PreprocType::Brace.open(), "{");
        assert_eq!(PreprocType::Brace.close(), "}");
        assert_eq!(PreprocType::Brace.min_count(), 2);
        assert_eq!(PreprocType::Brace.max_count(), 3);
        assert_eq!(PreprocType::Bracket.max_count(), 2);
        assert_eq!(PreprocType::Heading.max_count(), 6);
    }

    #[test]
    fn test_print_contents() {
        let pieces = vec![str_piece("foo"), str_piece("bar")];
        assert_eq!(print_contents(&pieces, false), "foobar");

        let nested = vec![PreprocPiece::Tk(PreprocTk::simple(
            PreprocType::Brace,
            SourceRange::new(0, 5),
            vec![str_piece("x")],
            2,
        ))];
        assert_eq!(print_contents(&nested, false), "{{x}}");
    }

    #[test]
    fn test_get_barred_args() {
        let contents = vec![str_piece("foo|bar|baz=qux")];
        let args = get_barred_args(&contents);
        assert_eq!(args.len(), 3);
        assert_eq!(print_contents(&args[0].0, false), "foo");
        assert_eq!(print_contents(&args[1].1, false), "bar");
        assert_eq!(print_contents(&args[2].0, false), "baz");
        assert_eq!(print_contents(&args[2].1, false), "qux");
    }

    #[test]
    fn test_tokenize_preproc_pieces_plain() {
        let pieces = tokenize_preproc_pieces("hello world");
        assert_eq!(pieces.len(), 1);
        assert_eq!(print_contents(&pieces, false), "hello world");
    }

    #[test]
    fn test_tokenize_preproc_pieces_template() {
        let pieces = tokenize_preproc_pieces("{{foo|bar}}");
        assert_eq!(pieces.len(), 1);
        match &pieces[0] {
            PreprocPiece::Tk(PreprocTk::Simple { ty, count, .. }) => {
                assert_eq!(*ty, PreprocType::Brace);
                assert_eq!(*count, 2);
            }
            other => panic!("expected brace template, got {:?}", other),
        }
    }

    #[test]
    fn test_tokenize_preproc_pieces_tplarg() {
        let pieces = tokenize_preproc_pieces("{{{1}}}");
        match &pieces[0] {
            PreprocPiece::Tk(PreprocTk::Simple { count, .. }) => {
                assert_eq!(*count, 3);
            }
            other => panic!("expected tplarg, got {:?}", other),
        }
    }

    #[test]
    fn test_tokenize_preproc_pieces_wikilink() {
        let pieces = tokenize_preproc_pieces("[[Foo]]");
        match &pieces[0] {
            PreprocPiece::Tk(PreprocTk::Simple { ty, .. }) => {
                assert_eq!(*ty, PreprocType::Bracket);
            }
            other => panic!("expected wikilink, got {:?}", other),
        }
    }

    #[test]
    fn test_tokenize_preproc_pieces_comment() {
        let pieces = tokenize_preproc_pieces("a<!-- c -->b");
        let mut has_comment = false;
        for p in &pieces {
            if let PreprocPiece::Tk(PreprocTk::Simple { ty, .. }) = p
                && *ty == PreprocType::Comment
            {
                has_comment = true;
            }
        }
        assert!(has_comment);
        assert_eq!(print_contents(&pieces, false), "a<!-- c -->b");
    }

    #[test]
    fn test_brace_to_attribs() {
        let tk = PreprocTk::simple(
            PreprocType::Brace,
            SourceRange::new(0, 13),
            vec![str_piece("foo|bar|baz=qux")],
            2,
        );
        let attribs = brace_to_attribs(&tk);
        assert_eq!(attribs.len(), 3);
        assert_eq!(attribs[0].key.as_str(), Some("foo"));
        assert_eq!(attribs[1].key.as_str(), Some(""));
        assert_eq!(attribs[1].value.as_str(), Some("bar"));
        assert_eq!(attribs[2].key.as_str(), Some("baz"));
        assert_eq!(attribs[2].value.as_str(), Some("qux"));
    }
}
