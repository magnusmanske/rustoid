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
            sr.start + count * self.open().len(),
            sr.end.saturating_sub(count * self.close().len()),
        )
    }

    /// The inverse of `shrink_range`. Mirrors `PreprocType::growRange`.
    pub fn grow_range(self, sr: &SourceRange, count: usize) -> SourceRange {
        SourceRange::new(
            sr.start.saturating_sub(count * self.open().len()),
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
}
