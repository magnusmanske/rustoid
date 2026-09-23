//! A Lua pattern matcher over Unicode codepoints.
//!
//! This is a port of Lua 5.1's `lstrlib.c`, which is the normative description
//! of the pattern language, with two deliberate differences that Scribunto's
//! `mw.ustring` requires:
//!
//! - the subject is walked by **codepoint** rather than byte, so `.` matches one
//!   character and quantifiers apply to whole characters;
//! - the single-letter classes are Unicode properties rather than ASCII (see
//!   [`super::classes`]).
//!
//! Porting the C rather than designing from the manual matters for the corners
//! the manual does not spell out: greedy-vs-minimal backtracking order, the
//! `%f` frontier rule, the maximum capture count, and how an empty capture
//! reports a position. Those are all decided by the C code, and a
//! reimplementation that "looks equivalent" diverges on real patterns.
//!
//! Two Lua 5.1 behaviours are deliberately *not* reproduced, because the service
//! does not have them:
//!
//! - a NUL byte inside a set is an ordinary character here. Lua 5.1's `classend`
//!   scans until `*p == '\0'` and raises "malformed pattern (missing ']')",
//!   which is what broke `Module:Citation/CS1`; the service returns a normal
//!   result, so `mw.ustring` must not inherit the bug.
//! - `\0` in the subject is likewise a character, not a terminator.

use super::classes::LuaClass;

/// The largest number of captures Lua's C implementation allows.
///
/// `LUA_MAXCAPTURES` in `lstrlib.c`. Exceeding it is an error rather than a
/// silent truncation, because a pattern that captures more than this is a
/// different pattern than the author wrote.
const MAX_CAPTURES: usize = 32;

/// The maximum number of recursion levels, mirroring `MAXCCALLS`.
///
/// Lua's matcher is recursive and relies on a C stack limit to stop runaway
/// backtracking. Without an equivalent cap, a pathological pattern would
/// overflow the Rust stack — a crash rather than an error, which is the one
/// outcome worse than a wrong answer.
const MAX_DEPTH: usize = 200;

/// A pattern-matching failure.
///
/// Lua reports these as errors with a specific wording, and the wording is
/// observable: `Module:Citation/CS1` catches them and some modules match on the
/// text, so they are reproduced rather than translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// `malformed pattern (ends with '%')`
    EndsWithEscape,
    /// `malformed pattern (missing ']')`
    MissingBracket,
    /// `invalid pattern capture index`
    InvalidCaptureIndex,
    /// `unfinished capture`
    UnfinishedCapture,
    /// `invalid capture index %N`
    InvalidCaptureRef(usize),
    /// `too many captures`
    TooManyCaptures,
    /// `pattern too complex`
    PatternTooComplex,
    /// `missing '[' after '%f' in pattern`
    MissingFrontierBracket,
    /// `invalid class` — a `%f` whose set is malformed.
    InvalidFrontierClass,
}

impl MatchError {
    /// The message Lua would raise, for a module that reads it.
    pub fn message(&self) -> String {
        match self {
            Self::EndsWithEscape => "malformed pattern (ends with '%')".into(),
            Self::MissingBracket => "malformed pattern (missing ']')".into(),
            Self::InvalidCaptureIndex => "invalid pattern capture".into(),
            Self::UnfinishedCapture => "unfinished capture".into(),
            Self::InvalidCaptureRef(n) => format!("invalid capture index %{n}"),
            Self::TooManyCaptures => "too many captures".into(),
            Self::PatternTooComplex => "pattern too complex".into(),
            Self::MissingFrontierBracket => "missing '[' after '%f' in pattern".into(),
            Self::InvalidFrontierClass => "invalid class in '%f'".into(),
        }
    }
}

/// What a capture holds: a span of the subject, or a position in it.
///
/// Lua distinguishes these because `()` captures the *position* rather than the
/// text, and the two print differently (`tostring` of a position capture is a
/// number, not a string). `Module:Citation/CS1` relies on it — its
/// `has_invisible_chars` compares a capture against `'nowiki'`, which only works
/// if a text capture is a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture<'a> {
    /// A matched span of the subject.
    Text(&'a str),
    /// A position, 1-based in **codepoints** (Scribunto's offsets are
    /// codepoints, unlike Lua's byte positions).
    Position(usize),
}

/// The result of a successful match.
#[derive(Debug, Clone)]
pub struct MatchResult<'a> {
    /// Where the whole match starts, as a 0-based codepoint index.
    pub start_index: usize,
    /// The whole matched span.
    pub whole: &'a str,
    /// The captures, in order.
    pub captures: Vec<Option<Capture<'a>>>,
}

/// The pattern engine for one match attempt.
///
/// Holds the subject as a codepoint vector, because every index in the algorithm
/// is a codepoint index and translating from byte offsets on every step would
/// both obscure the port and be slow.
struct Matcher<'a> {
    /// The subject, as codepoints, with each character's byte offset parallel.
    chars: Vec<char>,
    /// Byte offset of each element of `chars`, plus one past the end.
    offsets: Vec<usize>,
    /// The subject text, for slicing captures out.
    text: &'a str,
    /// Captures, indexed from 1 as Lua does (element 0 is unused).
    captures: [Option<CaptureSpan>; MAX_CAPTURES],
    /// Recursion depth, to bound backtracking.
    depth: usize,
    /// Set when a capture index turned out to be invalid, which in Lua's
    /// `end_capture` is raised rather than returned.
    error: Option<MatchError>,
}

/// A capture's span while matching, before it is turned into a [`Capture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CaptureSpan {
    start: usize,
    /// `None` while the capture is still open.
    end: Option<usize>,
    /// A `()` capture, which spans nothing and reports its start position.
    position: bool,
}

/// Sentinel for "no capture", matching `CAP_UNFINISHED`/`CAP_POSITION` being
/// encoded as negative lengths in the C.
impl CaptureSpan {
    fn is_open(&self) -> bool {
        self.end.is_none()
    }
}

impl<'a> Matcher<'a> {
    fn new(text: &'a str) -> Self {
        let mut chars = Vec::new();
        let mut offsets = Vec::new();
        for (byte, c) in text.char_indices() {
            chars.push(c);
            offsets.push(byte);
        }
        offsets.push(text.len());
        Self {
            chars,
            offsets,
            text,
            captures: [None; MAX_CAPTURES],
            depth: 0,
            error: None,
        }
    }

    /// The subject length in codepoints.
    fn len(&self) -> usize {
        self.chars.len()
    }

    /// The character at codepoint index `i`, or `None` past the end.
    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// The text of codepoints `start..end`.
    fn slice(&self, start: usize, end: usize) -> &'a str {
        &self.text[self.offsets[start]..self.offsets[end]]
    }
}

/// Compile-and-run entry point: try to match `pattern` in `text`, starting the
/// search at codepoint `init`.
///
/// Returns `Ok(None)` for "no match", which is what every `mw.ustring` function
/// reports, and `Err` only for a malformed pattern.
pub fn find_match<'a>(
    text: &'a str,
    pattern: &str,
    init: usize,
    plain: bool,
) -> Result<Option<MatchResult<'a>>, MatchError> {
    if plain {
        return Ok(plain_find(text, pattern, init));
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let mut m = Matcher::new(text);
    // Lua's `str_find_aux`: the subject is tried at each start position in turn,
    // unless the pattern is anchored with `^`, which permits only the first.
    let anchored = pattern.first() == Some(&'^');
    let p_start = usize::from(anchored);
    let mut s = init;
    loop {
        m.captures = [None; MAX_CAPTURES];
        m.depth = 0;
        m.error = None;
        let res = m.match_(&pattern, p_start, s);
        if let Some(end) = res {
            return Ok(Some(m.finish(s, end)));
        }
        if let Some(e) = m.error.clone() {
            return Err(e);
        }
        // An anchored pattern is tried only at `init`, so a failure there is
        // final. Without this the search would slide forward and `^b` would
        // wrongly match `abc` at its second character.
        if anchored {
            return Ok(None);
        }
        // `s < len` in the C; an empty subject still gets one attempt at 0.
        if s >= m.len() {
            return Ok(None);
        }
        s += 1;
    }
}

/// A literal search, for `plain = true`.
fn plain_find<'a>(text: &'a str, pattern: &str, init: usize) -> Option<MatchResult<'a>> {
    if pattern.is_empty() {
        return None;
    }
    let from = text
        .char_indices()
        .nth(init)
        .map(|(b, _)| b)
        .unwrap_or(text.len());
    let hay = &text[from..];
    let byte = hay.find(pattern)?;
    let start_char = text[..from + byte].chars().count();
    let end_char = start_char + pattern.chars().count();
    let whole = &text[from + byte..from + byte + pattern.len()];
    let _ = end_char;
    Some(MatchResult {
        start_index: start_char,
        whole,
        captures: Vec::new(),
    })
}

impl<'a> Matcher<'a> {
    /// Turn a successful match into its result, resolving captures to text.
    fn finish(&self, start: usize, end: usize) -> MatchResult<'a> {
        let mut captures = Vec::new();
        for slot in &self.captures {
            match slot {
                None => break,
                Some(c) => {
                    if c.position {
                        // Lua reports a position capture as a 1-based index.
                        captures.push(Some(Capture::Position(c.start + 1)));
                    } else {
                        let stop = c.end.unwrap_or(end);
                        captures.push(Some(Capture::Text(self.slice(c.start, stop))));
                    }
                }
            }
        }
        MatchResult {
            start_index: start,
            whole: self.slice(start, end),
            captures,
        }
    }

    /// `match` in `lstrlib.c`: whether `pattern[p..]` matches at `s`, returning
    /// the subject index one past the match.
    fn match_(&mut self, pattern: &[char], p: usize, s: usize) -> Option<usize> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.error = Some(MatchError::PatternTooComplex);
            return None;
        }
        let result = self.match_inner(pattern, p, s);
        self.depth -= 1;
        result
    }

    fn match_inner(&mut self, pattern: &[char], p: usize, s: usize) -> Option<usize> {
        let mut p = p;
        let mut s = s;
        loop {
            if p >= pattern.len() {
                return Some(s);
            }
            match pattern[p] {
                // `(` opens a capture, and `()` opens a position capture.
                '(' => {
                    return if pattern.get(p + 1) == Some(&')') {
                        self.start_capture(pattern, p + 2, s, true)
                    } else {
                        self.start_capture(pattern, p + 1, s, false)
                    };
                }
                ')' => return self.end_capture(pattern, p, s),
                // `$` at the very end anchors to the end of the subject.
                '$' if p + 1 == pattern.len() => {
                    return (s == self.len()).then_some(s);
                }
                '%' => {
                    if p + 1 >= pattern.len() {
                        self.error = Some(MatchError::EndsWithEscape);
                        return None;
                    }
                    match pattern[p + 1] {
                        // `%bxy` — a balanced match between x and y.
                        'b' => return self.match_balance(pattern, p + 2, s),
                        // `%f[set]` — a frontier pattern.
                        'f' => {
                            if pattern.get(p + 2) != Some(&'[') {
                                self.error = Some(MatchError::MissingFrontierBracket);
                                return None;
                            }
                            let (class_end, _) = self.class_end(pattern, p + 2)?;
                            return self.match_frontier(pattern, p + 2, class_end, s);
                        }
                        // A back-reference `%1`..`%9`.
                        d if d.is_ascii_digit() => return self.match_capture(pattern, p, s, d),
                        // Any other `%x` is a class, a literal escape, or `%%`,
                        // and it takes the same path as any other pattern item —
                        // this is the C's `goto dflt`, and it is what lets `%a+`
                        // reach the quantifier handling.
                        _ => {}
                    }
                }
                // Any other item: a class or a literal, plus an optional
                // quantifier.
                _ => {}
            }
            // The item and its quantifier. A class ends at `ep`; the quantifier,
            // if present, is the character straight after it.
            let (ep, _) = self.class_end(pattern, p)?;
            match pattern.get(ep) {
                Some('?') => {
                    // One if it matches, otherwise none — this order is what
                    // makes `?` prefer to consume, as in the C.
                    if self.single_match(pattern, p, ep, s)
                        && let Some(r) = self.match_(pattern, ep + 1, s + 1)
                    {
                        return Some(r);
                    }
                    p = ep + 1;
                }
                Some('*') => return self.max_expand(pattern, p, ep, s),
                Some('+') => {
                    return if self.single_match(pattern, p, ep, s) {
                        self.max_expand(pattern, p, ep, s + 1)
                    } else {
                        None
                    };
                }
                Some('-') => return self.min_expand(pattern, p, ep, s),
                _ => {
                    if !self.single_match(pattern, p, ep, s) {
                        return None;
                    }
                    p = ep;
                    s += 1;
                }
            }
        }
    }

    /// `max_expand` — the greedy `*` and `+`.
    ///
    /// Counts how many times the class matches, then backtracks from that
    /// maximum down to zero. The order matters more than it looks: it is what
    /// makes `.*` consume as much as possible while still letting the rest of
    /// the pattern match, and reversing it changes which capture wins.
    fn max_expand(&mut self, pattern: &[char], p: usize, ep: usize, s: usize) -> Option<usize> {
        let mut count = 0;
        while self.single_match(pattern, p, ep, s + count) {
            count += 1;
        }
        loop {
            if let Some(r) = self.match_(pattern, ep + 1, s + count) {
                return Some(r);
            }
            if count == 0 {
                return None;
            }
            count -= 1;
        }
    }

    /// `min_expand` — the lazy `-`.
    ///
    /// Tries the rest of the pattern first and only then consumes one more
    /// character, which is the opposite order to `max_expand`.
    fn min_expand(&mut self, pattern: &[char], p: usize, ep: usize, s: usize) -> Option<usize> {
        let mut s = s;
        loop {
            if let Some(r) = self.match_(pattern, ep + 1, s) {
                return Some(r);
            }
            if self.single_match(pattern, p, ep, s) {
                s += 1;
            } else {
                return None;
            }
        }
    }

    /// `classend`: the index just past the class starting at `p`.
    ///
    /// Returns the end and whether it was a set (`[...]`), which the frontier
    /// case needs. The quantifier characters are *not* consumed here — the
    /// caller's `max_expand`/`min_expand` loop handles them, as in the C.
    fn class_end(&mut self, pattern: &[char], p: usize) -> Option<(usize, bool)> {
        let c = *pattern.get(p)?;
        let mut p = p + 1;
        match c {
            '%' => {
                if p >= pattern.len() {
                    self.error = Some(MatchError::EndsWithEscape);
                    return None;
                }
                Some((p + 1, false))
            }
            '[' => {
                if pattern.get(p) == Some(&'^') {
                    p += 1;
                }
                // The first character is taken literally even if it is `]`, so
                // the scan starts one past it. Unlike Lua 5.1, a NUL does not
                // terminate the scan: the service treats NUL as ordinary, and
                // that difference is what `Module:Citation/CS1` depends on.
                let mut first = true;
                loop {
                    let Some(&cur) = pattern.get(p) else {
                        self.error = Some(MatchError::MissingBracket);
                        return None;
                    };
                    p += 1;
                    if cur == ']' && !first {
                        return Some((p, true));
                    }
                    first = false;
                    // `%]` inside a set escapes the bracket.
                    if cur == '%' {
                        if pattern.get(p).is_some() {
                            p += 1;
                        } else {
                            self.error = Some(MatchError::MissingBracket);
                            return None;
                        }
                    }
                }
            }
            _ => Some((p, false)),
        }
    }

    /// `singlematch`: does the class at `p..ep` match the subject at `s`?
    ///
    /// A past-the-end subject never matches, which is how `.` and classes fail
    /// at the end of the subject without a bounds error.
    fn single_match(&self, pattern: &[char], p: usize, ep: usize, s: usize) -> bool {
        let Some(subject_char) = self.at(s) else {
            return false;
        };
        match pattern[p] {
            '.' => true,
            '%' => {
                let class_char = pattern[p + 1];
                match LuaClass::from_letter(class_char) {
                    Some(class) => class.matches(subject_char),
                    // Not a class: a literal escape comparing equal to the char.
                    None => class_char == subject_char,
                }
            }
            '[' => self.match_bracket_class(pattern, p, ep, subject_char),
            other => other == subject_char,
        }
    }

    /// Whether `c` is in the set at `p..ep`.
    fn match_bracket_class(&self, pattern: &[char], p: usize, ep: usize, c: char) -> bool {
        let mut p = p + 1;
        let mut negate = false;
        if pattern.get(p) == Some(&'^') {
            negate = true;
            p += 1;
        }
        // `ep` is one past the `]`, so the last content char is `ep - 2`.
        let last = ep.saturating_sub(1);
        while p < last {
            if pattern[p] == '%' && p + 1 < last {
                p += 1;
                let cl = pattern[p];
                if let Some(class) = LuaClass::from_letter(cl) {
                    if class.matches(c) {
                        return !negate;
                    }
                } else if cl == c {
                    return !negate;
                }
                p += 1;
            } else if p + 2 < last && pattern[p + 1] == '-' {
                // A range, `a-z`.
                let (lo, hi) = (pattern[p], pattern[p + 2]);
                if lo <= c && c <= hi {
                    return !negate;
                }
                p += 3;
            } else {
                if pattern[p] == c {
                    return !negate;
                }
                p += 1;
            }
        }
        negate
    }

    /// `matchbalance` — `%bxy`.
    fn match_balance(&mut self, pattern: &[char], p: usize, s: usize) -> Option<usize> {
        let (open, close) = (pattern.get(p)?, pattern.get(p + 1)?);
        if self.at(s) != Some(*open) {
            return None;
        }
        let mut cont = 1;
        let mut s = s + 1;
        while s < self.len() {
            let c = self.at(s)?;
            if c == *close {
                cont -= 1;
                if cont == 0 {
                    // Continue matching *after* the balanced run.
                    return self.match_(pattern, p + 2, s + 1);
                }
            } else if c == *open {
                cont += 1;
            }
            s += 1;
        }
        None
    }

    /// `match_capture` — a back-reference `%1`..`%9`.
    ///
    /// The digit is 1-based while the capture slots are 0-based, so `%1` refers
    /// to slot 0 — this is `check_capture`'s `l -= '1'` in the C, and getting it
    /// wrong makes every back-reference read the wrong capture.
    fn match_capture(
        &mut self,
        pattern: &[char],
        p: usize,
        s: usize,
        digit: char,
    ) -> Option<usize> {
        let index = digit.to_digit(10).map(|d| d as usize - 1)?;
        let capture = self.captures.get(index).and_then(|c| c.as_ref())?;
        if capture.is_open() {
            self.error = Some(MatchError::UnfinishedCapture);
            return None;
        }
        let len = capture.end.unwrap_or(s) - capture.start;
        if self.len() - s >= len {
            // Compare the captured text against the subject at `s`.
            let captured: Vec<char> = self.chars[capture.start..capture.start + len].to_vec();
            let subject: Vec<char> = self.chars[s..s + len].to_vec();
            if captured == subject {
                return self.match_(pattern, p + 2, s + len);
            }
        }
        None
    }

    /// The frontier pattern, `%f[set]`.
    ///
    /// Matches an empty string at a position where the previous character is not
    /// in the set and the current one is — which is how Lua spells "starts a
    /// word". Out-of-range positions read as `\0`, exactly as the C does, so a
    /// frontier can match at the very start and end of the subject.
    fn match_frontier(&mut self, pattern: &[char], p: usize, ep: usize, s: usize) -> Option<usize> {
        let previous = if s > 0 { self.at(s - 1) } else { None };
        let current = self.at(s);
        let prev_in = previous.map(|c| self.match_bracket_class(pattern, p, ep, c));
        let curr_in = current.map(|c| self.match_bracket_class(pattern, p, ep, c));
        // `\0` is not in any set, which is what makes the boundary work.
        if prev_in == Some(true) || curr_in != Some(true) {
            return None;
        }
        // Consume nothing.
        self.match_(pattern, ep, s)
    }

    /// `start_capture`.
    ///
    /// Returns the match result, *including* failure: a capture whose body does
    /// not match fails the whole item, and reporting `Some` here would turn
    /// every failed capture into an empty match.
    fn start_capture(
        &mut self,
        pattern: &[char],
        p: usize,
        s: usize,
        position: bool,
    ) -> Option<usize> {
        let level = self
            .captures
            .iter()
            .position(|c| c.is_none())
            .unwrap_or(MAX_CAPTURES);
        if level >= MAX_CAPTURES {
            self.error = Some(MatchError::TooManyCaptures);
            return None;
        }
        self.captures[level] = Some(CaptureSpan {
            start: s,
            end: if position { Some(s) } else { None },
            position,
        });
        match self.match_(pattern, p, s) {
            Some(end) => Some(end),
            None => {
                // The capture is abandoned with the failed branch.
                self.captures[level] = None;
                None
            }
        }
    }

    /// `end_capture`.
    fn end_capture(&mut self, pattern: &[char], p: usize, s: usize) -> Option<usize> {
        // Find the innermost open capture.
        let level = self
            .captures
            .iter()
            .rposition(|c| c.as_ref().is_some_and(|c| c.is_open()))
            .or_else(|| {
                self.error = Some(MatchError::InvalidCaptureIndex);
                None
            })?;
        if let Some(c) = self.captures[level].as_mut() {
            c.end = Some(s);
        }
        match self.match_(pattern, p + 1, s) {
            Some(end) => Some(end),
            None => {
                if let Some(c) = self.captures[level].as_mut() {
                    c.end = None;
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Match `pattern` against `text`, requiring a match at or after `init`, and
    /// return the whole match text plus captures as strings.
    fn m(text: &str, pattern: &str) -> Option<(String, Vec<String>)> {
        let r = find_match(text, pattern, 0, false).unwrap()?;
        let caps = r
            .captures
            .iter()
            .map(|c| match c {
                Some(Capture::Text(t)) => t.to_string(),
                Some(Capture::Position(p)) => p.to_string(),
                None => "nil".to_string(),
            })
            .collect();
        Some((r.whole.to_string(), caps))
    }

    #[test]
    fn literals_and_dot() {
        assert_eq!(m("abc", "b").unwrap().0, "b");
        assert_eq!(m("abc", "a.c").unwrap().0, "abc");
        assert_eq!(m("abc", "x"), None);
        // `.` matches one *character*, so a two-byte é is a single match.
        assert_eq!(m("é", ".").unwrap().0, "é");
    }

    #[test]
    fn anchors() {
        assert_eq!(m("abc", "^a").unwrap().0, "a");
        assert_eq!(m("abc", "^b"), None);
        assert_eq!(m("abc", "c$").unwrap().0, "c");
        assert_eq!(m("abc", "b$"), None);
    }

    #[test]
    fn quantifiers_are_greedy_then_minimal() {
        assert_eq!(m("abcabc", ".*").unwrap().0, "abcabc");
        assert_eq!(m("abcabc", ".-").unwrap().0, "");
        // Greedy takes as much as it can while still allowing the rest to match.
        assert_eq!(m("aXbXc", "%a.*%a").unwrap().0, "aXbXc");
        // Minimal takes as little, so the second `%a` matches the first letter
        // it can — the `X` right after `a`. Verified against the service as
        // `mw.ustring.match('aXbXc', 'a.-%a')` -> `aX`.
        assert_eq!(m("aXbXc", "a.-%a").unwrap().0, "aX");
        assert_eq!(m("aXbXc", "%a.-%a").unwrap().0, "aX");
    }

    #[test]
    fn captures_and_backreferences() {
        let (_, caps) = m("abc123", "(%a+)(%d+)").unwrap();
        assert_eq!(caps, vec!["abc", "123"]);
        // A back-reference must match the same text again.
        assert_eq!(m("aa", "(a)%1").unwrap().0, "aa");
        assert_eq!(m("ab", "(a)%1"), None);
        // Nested captures close innermost-first, so the outer contains the inner.
        let (_, caps) = m("abc", "((a)(b))").unwrap();
        assert_eq!(caps, vec!["ab", "a", "b"]);
    }

    #[test]
    fn position_captures_are_one_based_codepoints() {
        let (_, caps) = m("abc", "()b()").unwrap();
        assert_eq!(caps, vec!["2", "3"]);
        // With a multibyte lead, the position is still a codepoint index.
        let (_, caps) = m("él", "()l").unwrap();
        assert_eq!(caps, vec!["2"]);
    }

    #[test]
    fn balanced_match() {
        assert_eq!(m("(a(b)c)d", "%b()").unwrap().0, "(a(b)c)");
        assert_eq!(m("((()))", "%b()").unwrap().0, "((()))");
    }

    #[test]
    fn frontier() {
        // `%f[%a]` is "starts a word": the character before is not a letter.
        // At position 0 the previous character is out of range, which reads as
        // `\0` — not a letter — so the frontier matches at the very start of the
        // subject. Verified: the service gives `xword` for `%f[%a]%a+` on
        // `xword`, and `word` for the same pattern on `'  word'`.
        assert_eq!(m("xword", "%f[%a]%a+").unwrap().0, "xword");
        assert_eq!(m("  word", "%f[%a]%a+").unwrap().0, "word");
    }

    #[test]
    fn sets_and_ranges() {
        assert_eq!(m("abc", "[abc]").unwrap().0, "a");
        assert_eq!(m("abc", "[^abc]"), None);
        assert_eq!(m("xyz", "[a-z]").unwrap().0, "x");
        // A `]` right after `[` is a literal, not the set's end.
        assert_eq!(m("]", "[]]").unwrap().0, "]");
        // Escapes inside a set.
        assert_eq!(m("%", "[%%]").unwrap().0, "%");
    }

    #[test]
    fn unicode_classes_apply() {
        // The cases verified against the service in `classes`.
        assert_eq!(m("héllo", "%a+").unwrap().0, "héllo");
        assert_eq!(m("１２３", "%d+").unwrap().0, "１２３");
        assert_eq!(m("１２３", "%x+").unwrap().0, "１２３");
        assert_eq!(m("　", "%s").unwrap().0, "　");
    }

    /// The bug that started this: a NUL inside a set must be an ordinary
    /// character. Lua 5.1 raises "malformed pattern (missing ']')" here; the
    /// service returns a normal result, so this must too.
    ///
    /// The pattern and subject are built with `char` literals rather than a
    /// Rust string literal, because Rust reads `\0` and `\8` as escapes and the
    /// point of the test is a NUL *codepoint* in the pattern.
    #[test]
    fn nul_in_a_set_is_ordinary() {
        let pattern: String = vec!['[', '\0', ']'].into_iter().collect();
        let subject: String = vec!['a', '\0', 'b'].into_iter().collect();
        let (whole, _) = m(&subject, &pattern).unwrap();
        assert_eq!(whole, "\0");

        // The C0 range from `Module:Citation/CS1`, which is the real case.
        let pattern: String = "[\u{0}-\u{8}\u{b}\u{c}\u{e}-\u{1f}]".to_string();
        let (whole, _) = m("a\u{1}b", &pattern).unwrap();
        assert_eq!(whole, "\u{1}");
        // And it must still compile when it does *not* match, rather than
        // raising the way Lua 5.1 does.
        assert_eq!(m("abc", &pattern), None);
    }

    #[test]
    fn malformed_patterns_report_luas_message() {
        assert_eq!(
            find_match("abc", "%", 0, false).unwrap_err(),
            MatchError::EndsWithEscape
        );
        assert_eq!(
            find_match("abc", "[abc", 0, false).unwrap_err(),
            MatchError::MissingBracket
        );
    }

    #[test]
    fn plain_search_is_literal() {
        // Under `plain`, `%a` is the four characters, not a class.
        assert_eq!(
            find_match("x%ay", "%a", 0, true).unwrap().unwrap().whole,
            "%a"
        );
        assert!(find_match("abc", "%a", 0, true).unwrap().is_none());
    }
}
