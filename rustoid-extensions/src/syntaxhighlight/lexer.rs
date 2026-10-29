//! A small port of Pygments' `RegexLexer` engine.
//!
//! Only what the syntax highlighter needs: ordered regex rules per state, a
//! state stack, `bygroups` and `using(this, …)`, and Pygments' fallback of
//! emitting an unmatched newline as `Whitespace` (which resets the stack) or any
//! other unmatched byte as an `Error` token.
//!
//! The one thing Rust's `regex` cannot express is lookaround, so the few
//! assertions the wikitext lexer uses are declared as [`Guard`]s and checked
//! around the match. Headings use a backreference (`(={2,6})…\1`), which is also
//! outside `regex`, and are matched by [`Matcher::Subheading`]/[`Matcher::Heading`].

use std::collections::HashMap;

use regex::Regex;

/// Which Pygments token (i.e. CSS class) a rule emits.
#[derive(Clone, Copy)]
pub enum Action {
    /// Emit nothing (`bygroups`' `None`, or a `default()` transition).
    Nothing,
    /// One token over the whole match.
    Class(&'static str),
    /// `bygroups`: one action per capture group.
    Groups(&'static [GroupAction]),
}

/// One `bygroups` entry: a token class, or a nested `using(this, …)`.
#[derive(Clone, Copy)]
pub enum GroupAction {
    Tok(&'static str, usize),
    Relex(usize, &'static [&'static str]),
}

/// Pygments' `new_state`.
#[derive(Clone, Copy)]
pub enum Next {
    Stay,
    Pop,
    PopN(usize),
    Push(&'static str),
    /// Push two states (Pygments' `('outer', 'inner')` tuple).
    Push2(&'static str, &'static str),
    /// `('#pop', 'state')`: pop, then push `state`.
    PopPush(&'static str),
}

/// A zero-width assertion around a match.
pub enum Guard {
    None,
    /// `(?=re)` at the end of the match.
    AtEnd(&'static str),
    /// `(?!re)` at the end of the match.
    NotAtEnd(&'static str),
    /// `(?!re)` `skip` bytes into the match (Pygments' `(\[\[)(?!…)`).
    NotAt(usize, &'static str),
}

/// What a rule matches.
pub enum Matcher {
    /// A regular expression, anchored at the current position.
    Re(&'static str),
    /// `^(={2,6})(.+?)(\1)(\s*$\n)` — a subheading, emitted as one token.
    Subheading,
    /// `^(=.+?=)(\s*$\n)` — a level-1 heading, emitted as one token.
    Heading,
    /// `(?<=\|)((?:(?!\{\{|\}\})[^=|<])*?)(=)` — a template argument's label.
    ///
    /// The scan excludes `<`, `|`, `=`, and the start of a template/argument,
    /// which `regex` cannot express without a lookahead per character.
    Label,
    /// `(\{\{\{)([^|]*?)(?=\}\}\}|\|)` — a template parameter's name. The
    /// lazy name has to stop *before* its terminator, which a guard at the
    /// (fixed) end of a regex match cannot express.
    Parameter,
    /// `^(:*)(\s*?)(\{\|)([^\n]*)$` — a table start line.
    TableStart,
    /// `^([ \t\n\r\0\x0B]*?)(\|\})` — a table end line.
    TableEnd,
    /// `^([ \t\n\r\0\x0B]*?)(\|-+)(.*)$` — a table row line.
    TableRow,
    /// `^([ \t\n\r\0\x0B]*?)(\|\+)…` — a table caption.
    TableCaption,
    /// `( ^(?:[ \t\n\r\0\x0B]*?)\| | \|\| )…` — a table data cell.
    TableCell,
    /// `( ^(?:[ \t\n\r\0\x0B]*?)! )…` — a table header cell.
    TableHeader,
    /// `(!!|\|\|)…` inside `table-header`.
    TableHeaderSep,
}

pub struct Rule {
    pub matcher: Matcher,
    pub guard: Guard,
    pub action: Action,
    pub next: Next,
}

impl Rule {
    pub const fn rx(re: &'static str, class: &'static str) -> Self {
        Self {
            matcher: Matcher::Re(re),
            guard: Guard::None,
            action: Action::Class(class),
            next: Next::Stay,
        }
    }

    pub const fn rx_next(re: &'static str, class: &'static str, next: Next) -> Self {
        Self {
            matcher: Matcher::Re(re),
            guard: Guard::None,
            action: Action::Class(class),
            next,
        }
    }

    pub const fn groups(re: &'static str, groups: &'static [GroupAction], next: Next) -> Self {
        Self {
            matcher: Matcher::Re(re),
            guard: Guard::None,
            action: Action::Groups(groups),
            next,
        }
    }

    pub const fn label(next: Next) -> Self {
        Self {
            matcher: Matcher::Label,
            guard: Guard::None,
            action: Action::Groups(&[GroupAction::Tok("nl", 1), GroupAction::Tok("o", 2)]),
            next,
        }
    }

    pub const fn parameter(next: Next) -> Self {
        Self {
            matcher: Matcher::Parameter,
            guard: Guard::None,
            action: Action::Groups(&[GroupAction::Tok("p", 1), GroupAction::Tok("nv", 2)]),
            next,
        }
    }

    pub const fn nothing(re: &'static str, next: Next) -> Self {
        Self {
            matcher: Matcher::Re(re),
            guard: Guard::None,
            action: Action::Nothing,
            next,
        }
    }

    #[must_use]
    pub const fn guard(mut self, guard: Guard) -> Self {
        self.guard = guard;
        self
    }
}

pub(super) struct Compiled {
    matcher: Matcher,
    re: Option<Regex>,
    guard: Option<(bool, Regex)>,
    guard_at: Option<(usize, bool, Regex)>,
    action: Action,
    next: Next,
}

/// Compile a lexer's states once.
pub fn compile(table: fn(&str) -> Vec<Rule>) -> HashMap<&'static str, Vec<Compiled>> {
    let mut states = HashMap::new();
    for name in crate::syntaxhighlight::wikitext::STATE_NAMES {
        let rules = table(name)
            .into_iter()
            .filter_map(|rule| {
                let re = match &rule.matcher {
                    Matcher::Re(re) => Some(compile_re(re)?),
                    _ => None,
                };
                let (guard, guard_at) = match &rule.guard {
                    Guard::None => (None, None),
                    Guard::AtEnd(re) => (Some((true, compile_re(re)?)), None),
                    Guard::NotAtEnd(re) => (Some((false, compile_re(re)?)), None),
                    Guard::NotAt(skip, re) => (None, Some((*skip, false, compile_re(re)?))),
                };
                Some(Compiled {
                    matcher: rule.matcher,
                    re,
                    guard,
                    guard_at,
                    action: rule.action,
                    next: rule.next,
                })
            })
            .collect();
        states.insert(*name, rules);
    }
    states
}

/// Pygments compiles every rule with `re.MULTILINE`.
///
/// A rule that does not compile is a porting error; it is dropped rather than
/// panicking at render time, and [`every_rule_compiles`] catches it in tests.
fn compile_re(pattern: &str) -> Option<Regex> {
    Regex::new(&format!("(?m){pattern}")).ok()
}

/// Lex `text` from `initial`, returning `(css class, slice)` tokens.
pub fn lex<'a>(
    text: &'a str,
    initial: &[&'static str],
    table: &HashMap<&'static str, Vec<Compiled>>,
) -> Vec<(&'static str, &'a str)> {
    let mut out = Vec::new();
    let mut stack: Vec<&'static str> = initial.to_vec();
    let mut pos = 0;
    while pos < text.len() {
        let rules = table
            .get(*stack.last().unwrap_or(&"root"))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut matched = false;
        for rule in rules {
            let Some(m) = matched_at(text, pos, rule) else {
                continue;
            };
            if let Some((positive, guard)) = &rule.guard {
                // `(?=re)` / `(?!re)` at the end of the match: the assertion
                // matches at that position, whatever its length.
                let hit = guard.find(&text[m.end..]).is_some_and(|g| g.start() == 0);
                if hit != *positive {
                    continue;
                }
            }
            if let Some((skip, positive, guard)) = &rule.guard_at {
                let at = m.start + skip;
                let hit =
                    at <= text.len() && guard.find(&text[at..]).is_some_and(|g| g.start() == 0);
                if hit != *positive {
                    continue;
                }
            }
            emit(text, pos, m.end, &m, rule.action, table, &mut out);
            apply(&rule.next, &mut stack);
            pos = m.end;
            matched = true;
            break;
        }
        if matched {
            continue;
        }
        let ch = text[pos..].chars().next().unwrap_or('\0');
        if ch == '\n' {
            // Pygments resets to `root` at a line the current state cannot lex.
            stack.truncate(1);
            out.push(("w", &text[pos..pos + 1]));
        } else {
            let end = pos + ch.len_utf8();
            out.push(("err", &text[pos..end]));
        }
        pos += ch.len_utf8();
    }
    out
}

struct Match<'a> {
    start: usize,
    end: usize,
    groups: Vec<(usize, usize)>,
    _text: &'a str,
}

fn matched_at<'a>(text: &'a str, pos: usize, rule: &Compiled) -> Option<Match<'a>> {
    match &rule.matcher {
        Matcher::Re(_) => {
            let re = rule.re.as_ref()?;
            let caps = re.captures_at(text, pos)?;
            let whole = caps.get(0)?;
            if whole.start() != pos {
                return None;
            }
            Some(Match {
                start: pos,
                end: whole.end(),
                groups: (0..caps.len())
                    .map(|i| {
                        caps.get(i)
                            .map(|g| (g.start(), g.end()))
                            .unwrap_or((whole.end(), whole.end()))
                    })
                    .collect(),
                _text: whole.as_str(),
            })
        }
        Matcher::Subheading => subheading(text, pos),
        Matcher::Heading => heading(text, pos),
        Matcher::Label => label(text, pos),
        Matcher::Parameter => parameter(text, pos),
        Matcher::TableStart => table_start(text, pos),
        Matcher::TableEnd => table_end(text, pos),
        Matcher::TableRow => table_row(text, pos),
        Matcher::TableCaption => table_caption(text, pos),
        Matcher::TableCell => table_cell(text, pos),
        Matcher::TableHeader => table_header(text, pos),
        Matcher::TableHeaderSep => table_header_sep(text, pos),
    }
}

/// The class Pygments writes as `[ \t\n\r\0\x0B]` (PHP's `trim()` set).
fn table_ws(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\0' | '\u{0B}')
}

fn at_line_start(text: &str, pos: usize) -> bool {
    pos == 0 || text[..pos].ends_with('\n')
}

/// The end of the current line (the newline itself excluded).
fn line_end(text: &str, pos: usize) -> usize {
    text[pos..]
        .find('\n')
        .map(|i| pos + i)
        .unwrap_or(text.len())
}

/// `^(?:[ws]*?)target`: the index of the first `target` reachable over
/// whitespace only, from a line start. `target` may be a two-character marker.
fn skip_ws_to(text: &str, pos: usize, target: &str) -> Option<usize> {
    if !at_line_start(text, pos) {
        return None;
    }
    let mut i = pos;
    loop {
        if text[i..].starts_with(target) {
            return Some(i);
        }
        let ch = text[i..].chars().next()?;
        if !table_ws(ch) {
            return None;
        }
        i += ch.len_utf8();
    }
}

/// `((?:(?!\[\[|\{\{)[^|\n<])*?)` followed by `sep`, optionally not
/// followed by `forbidden`. Returns the index of `sep`.
///
/// A run that meets `[[`, `{{`, `<`, a newline or a pipe fails, which makes the
/// enclosing optional group fail — the caller then ends the match earlier.
fn scan_attr_run(text: &str, from: usize, sep: char, forbidden: Option<char>) -> Option<usize> {
    let mut i = from;
    loop {
        if text[i..].starts_with(sep) {
            let after = i + sep.len_utf8();
            if forbidden.is_some_and(|c| text[after..].starts_with(c)) {
                return None;
            }
            return Some(i);
        }
        if text[i..].starts_with("[[") || text[i..].starts_with("{{") {
            return None;
        }
        let ch = text[i..].chars().next()?;
        if matches!(ch, '|' | '\n' | '<') {
            return None;
        }
        i += ch.len_utf8();
    }
}

/// A table start line, `^(:*)(\s*?)(\{\|)([^\n]*)$`.
fn table_start(text: &str, pos: usize) -> Option<Match<'_>> {
    if !at_line_start(text, pos) {
        return None;
    }
    let cols_end = text[pos..].bytes().take_while(|&b| b == b':').count() + pos;
    let pipe = skip_ws_to(text, cols_end, "{|")?;
    let end = line_end(text, pipe + 2);
    Some(Match {
        start: pos,
        end,
        groups: vec![
            (pos, end),
            (pos, cols_end),
            (cols_end, pipe),
            (pipe, pipe + 2),
            (pipe + 2, end),
        ],
        _text: &text[pos..end],
    })
}

/// A table end line, `^([ws]*?)(\|\})`.
fn table_end(text: &str, pos: usize) -> Option<Match<'_>> {
    let pipe = skip_ws_to(text, pos, "|}")?;
    Some(Match {
        start: pos,
        end: pipe + 2,
        groups: vec![(pos, pipe + 2), (pos, pipe), (pipe, pipe + 2)],
        _text: &text[pos..pipe + 2],
    })
}

/// A row line, `^([ws]*?)(\|-+)(.*)$`.
fn table_row(text: &str, pos: usize) -> Option<Match<'_>> {
    let pipe = skip_ws_to(text, pos, "|")?;
    let mut i = pipe + 1;
    while text[i..].starts_with('-') {
        i += 1;
    }
    if i == pipe + 1 {
        return None;
    }
    let end = line_end(text, i);
    Some(Match {
        start: pos,
        end,
        groups: vec![(pos, end), (pos, pipe), (pipe, i), (i, end)],
        _text: &text[pos..end],
    })
}

/// A caption line, `^([ws]*?)(\|+)` with an optional attribute box.
fn table_caption(text: &str, pos: usize) -> Option<Match<'_>> {
    let pipe = skip_ws_to(text, pos, "|")?;
    let plus_end = pipe + 1 + text[pipe + 1..].bytes().take_while(|&b| b == b'+').count();
    if plus_end == pipe + 1 {
        return None;
    }
    let end = line_end(text, plus_end);
    let mut groups = vec![
        (pos, end),
        (pos, pipe),
        (pipe, plus_end),
        (plus_end, plus_end),
        (plus_end, plus_end),
        (plus_end, end),
    ];
    if let Some(sep) = scan_attr_run(text, plus_end, '|', None) {
        groups[3] = (plus_end, sep);
        groups[4] = (sep, sep + 1);
        groups[5] = (sep + 1, end);
    }
    Some(Match {
        start: pos,
        end,
        groups,
        _text: &text[pos..end],
    })
}

/// A data cell's opener: a line-leading `|` (over whitespace) or a `||`.
fn table_cell(text: &str, pos: usize) -> Option<Match<'_>> {
    let (opener_end, _) = if at_line_start(text, pos)
        && let Some(pipe) = skip_ws_to(text, pos, "|")
    {
        (pipe + 1, false)
    } else if text[pos..].starts_with("||") {
        (pos + 2, true)
    } else {
        return None;
    };
    cell_match(text, pos, opener_end)
}

/// A header cell's opener, `( ^(?:[ws]*?)! )`.
fn table_header(text: &str, pos: usize) -> Option<Match<'_>> {
    let bang = skip_ws_to(text, pos, "!")?;
    cell_match(text, pos, bang + 1)
}

/// The optional `(attr)(\|)(?!\|)` after a cell opener.
fn cell_match(text: &str, pos: usize, opener_end: usize) -> Option<Match<'_>> {
    let mut end = opener_end;
    // Group 0 is the whole match; the three captures are opener, attribute run
    // and separator.
    let mut groups = vec![
        (pos, opener_end),
        (pos, opener_end),
        (opener_end, opener_end),
        (opener_end, opener_end),
    ];
    if let Some(sep) = scan_attr_run(text, opener_end, '|', Some('|')) {
        groups[2] = (opener_end, sep);
        groups[3] = (sep, sep + 1);
        end = sep + 1;
    }
    groups[0] = (pos, end);
    Some(Match {
        start: pos,
        end,
        groups,
        _text: &text[pos..end],
    })
}

/// `(!!|\|\|)` with the same optional attribute box, inside `table-header`.
fn table_header_sep(text: &str, pos: usize) -> Option<Match<'_>> {
    let opener = if text[pos..].starts_with("!!") {
        "!!"
    } else if text[pos..].starts_with("||") {
        "||"
    } else {
        return None;
    };
    cell_match(text, pos, pos + opener.len())
}

/// `(\{\{\{)([^|]*?)(?=\}\}\}|\|)`: a parameter's name, up to its terminator.
fn parameter(text: &str, pos: usize) -> Option<Match<'_>> {
    text.get(pos..)?.strip_prefix("{{{")?;
    let start = pos + 3;
    let mut i = start;
    while i < text.len() {
        if text[i..].starts_with("}}}") || text[i..].starts_with('|') {
            break;
        }
        i += text[i..].chars().next()?.len_utf8();
    }
    if i >= text.len() {
        return None;
    }
    Some(Match {
        start: pos,
        end: i,
        groups: vec![(pos, i), (pos, start), (start, i)],
        _text: &text[pos..i],
    })
}

/// `(?<=\|)((?:(?!\{\{|\}\})[^=|<])*?)(=)`: a template argument's label.
fn label(text: &str, pos: usize) -> Option<Match<'_>> {
    if !text[..pos].ends_with('|') {
        return None;
    }
    let mut i = pos;
    while i < text.len() {
        if text[i..].starts_with("{{") || text[i..].starts_with("}}") {
            return None;
        }
        let ch = text[i..].chars().next()?;
        if ch == '=' {
            return Some(Match {
                start: pos,
                end: i + 1,
                groups: vec![(pos, text.len()), (pos, i), (i, i + 1)],
                _text: &text[pos..i + 1],
            });
        }
        if matches!(ch, '|' | '<') {
            return None;
        }
        i += ch.len_utf8();
    }
    None
}

/// `^(={2,6})(.+?)(\1)(\s*$\n)`, without the backreference.
fn subheading(text: &str, pos: usize) -> Option<Match<'_>> {
    if pos > 0 && !text[..pos].ends_with('\n') {
        return None;
    }
    let run = text[pos..].bytes().take_while(|&b| b == b'=').count();
    if !(2..=6).contains(&run) {
        return None;
    }
    let content_start = pos + run;
    let line_end = text[pos..].find('\n').map(|i| pos + i)?;
    let content = text.get(content_start..line_end)?;
    if content.is_empty() {
        return None;
    }
    // The closing run is the last run of exactly `run` equals on the line.
    let closing = content.rfind(&"=".repeat(run))? + content_start;
    if closing == content_start {
        return None;
    }
    let after = &text[closing + run..line_end];
    if !after.chars().all(char::is_whitespace) {
        return None;
    }
    Some(Match {
        start: pos,
        end: closing + run,
        groups: vec![
            (pos, text.len()),
            (pos, content_start),
            (content_start, closing),
            (closing, closing + run),
        ],
        _text: &text[pos..closing + run],
    })
}

/// `^(=.+?=)(\s*$\n)` — a level-1 heading.
fn heading(text: &str, pos: usize) -> Option<Match<'_>> {
    if pos > 0 && !text[..pos].ends_with('\n') {
        return None;
    }
    if !text[pos..].starts_with('=') {
        return None;
    }
    let line_end = text[pos..].find('\n').map(|i| pos + i)?;
    let line = text.get(pos..line_end)?;
    let close = line.rfind('=')?;
    if close == 0 {
        return None;
    }
    Some(Match {
        start: pos,
        end: pos + close + 1,
        groups: vec![(pos, text.len()), (pos, pos + close + 1)],
        _text: &text[pos..pos + close + 1],
    })
}

fn emit<'a>(
    text: &'a str,
    start: usize,
    end: usize,
    m: &Match<'a>,
    action: Action,
    table: &HashMap<&'static str, Vec<Compiled>>,
    out: &mut Vec<(&'static str, &'a str)>,
) {
    match action {
        Action::Nothing => {}
        Action::Class(class) => {
            if start < end {
                out.push((class, &text[start..end]));
            }
        }
        Action::Groups(groups) => {
            for group in groups {
                match group {
                    GroupAction::Tok(class, index) => {
                        if let Some((gs, ge)) = m.groups.get(*index) {
                            let piece = &text[*gs..*ge];
                            if !piece.is_empty() {
                                out.push((class, piece));
                            }
                        }
                    }
                    GroupAction::Relex(index, stack) => {
                        if let Some((gs, ge)) = m.groups.get(*index) {
                            out.extend(lex(&text[*gs..*ge], stack, table));
                        }
                    }
                }
            }
        }
    }
}

fn apply(next: &Next, stack: &mut Vec<&'static str>) {
    match next {
        Next::Stay => {}
        Next::Pop => {
            if stack.len() > 1 {
                stack.pop();
            }
        }
        Next::PopN(n) => {
            if *n >= stack.len() {
                stack.truncate(1);
            } else {
                stack.truncate(stack.len() - *n);
            }
        }
        Next::Push(state) => stack.push(state),
        Next::Push2(outer, inner) => {
            stack.push(outer);
            stack.push(inner);
        }
        Next::PopPush(state) => {
            if stack.len() > 1 {
                stack.pop();
            }
            stack.push(state);
        }
    }
}
