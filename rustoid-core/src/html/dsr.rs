//! DOM Source Range (DSR) model for html2wt.
//!
//! Faithful port of PHP Parsoid's `Core\SourceRange` and
//! `Core\DomSourceRange` (the `DomSourceRange` extends `SourceRange`). These
//! carry (possibly null) source offsets, optional container tag widths, and
//! trimmed-whitespace widths, and provide the range arithmetic the separator
//! algorithm relies on (`innerStart`/`innerEnd`, `openRange`/`closeRange`/
//! `innerRange`, `to`, `offset`, `hasTrimmedWS`, …).
//!
//! This is a *serializer-facing* model distinct from the tokenizer's
//! non-nullable `tokens_v2::SourceRange`/`DomSourceRange`, because the
//! html2wt path must faithfully represent null DSR offsets (fostered/misnested
//! content) and trimmed-whitespace metadata that the tokenizer-side range does
//! not model.

/// A source offset range with a (possibly null) start/end and optional source
/// text. Mirrors PHP's `Core\SourceRange`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceRange {
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// The source text this range indexes into (PHP's `?Source`).
    pub source: Option<String>,
}

impl SourceRange {
    pub fn new(start: Option<usize>, end: Option<usize>) -> Self {
        Self {
            start,
            end,
            source: None,
        }
    }

    pub fn with_source(start: Option<usize>, end: Option<usize>, source: Option<String>) -> Self {
        Self { start, end, source }
    }

    /// Length of the range (`end - start`), or `0` if either is null.
    pub fn length(&self) -> usize {
        match (self.start, self.end) {
            (Some(s), Some(e)) if e >= s => e - s,
            _ => 0,
        }
    }

    /// Return a range spanning from this range's end to `sr`'s start.
    pub fn to(&self, sr: &SourceRange) -> SourceRange {
        SourceRange::with_source(self.end, sr.start, self.source.clone())
    }

    /// The substring of `input` covered by this range (or `""` when invalid).
    pub fn substr<'a>(&self, input: &'a str) -> &'a str {
        match (self.start, self.end) {
            (Some(start), Some(end)) if start <= end && start <= input.len() => {
                let end = end.min(input.len());
                &input[start..end]
            }
            _ => "",
        }
    }

    /// Shift both offsets by `amount`.
    pub fn offset(&self, amount: isize) -> SourceRange {
        let shift = |o: Option<usize>| o.and_then(|v| v.checked_add_signed(amount));
        SourceRange::with_source(shift(self.start), shift(self.end), self.source.clone())
    }
}

/// A DOM source range: a [`SourceRange`] plus optional container-tag widths and
/// trimmed-whitespace widths. Mirrors PHP's `Core\DomSourceRange`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomSourceRange {
    pub start: Option<usize>,
    pub end: Option<usize>,
    pub source: Option<String>,
    /// Opening tag width (`null` when unknown).
    pub open_width: Option<usize>,
    /// Closing tag width (`null` when unknown).
    pub close_width: Option<usize>,
    /// Width of trimmed whitespace between open tag & first child (`-1` invalid).
    pub leading_ws: isize,
    /// Width of trimmed whitespace between last child & close tag (`-1` invalid).
    pub trailing_ws: isize,
}

impl DomSourceRange {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        start: Option<usize>,
        end: Option<usize>,
        open_width: Option<usize>,
        close_width: Option<usize>,
        leading_ws: isize,
        trailing_ws: isize,
    ) -> Self {
        Self {
            start,
            end,
            source: None,
            open_width,
            close_width,
            leading_ws,
            trailing_ws,
        }
    }

    /// Inner start = start + open width.
    pub fn inner_start(&self) -> usize {
        self.start.unwrap_or(0) + self.open_width.unwrap_or(0)
    }

    /// Inner end = end - close width.
    pub fn inner_end(&self) -> usize {
        self.end
            .unwrap_or(0)
            .saturating_sub(self.close_width.unwrap_or(0))
    }

    /// Length of the inner range (`inner_end - inner_start`), saturating at 0.
    pub fn inner_length(&self) -> usize {
        self.inner_end().saturating_sub(self.inner_start())
    }

    /// Length of the full range (`end - start`), saturating at 0 (faithful to
    /// `SourceRange::length`, which `DomSourceRange` inherits in PHP).
    pub fn length(&self) -> usize {
        match (self.start, self.end) {
            (Some(s), Some(e)) if e >= s => e - s,
            _ => 0,
        }
    }

    /// Range of the open portion.
    pub fn open_range(&self) -> SourceRange {
        SourceRange::with_source(self.start, Some(self.inner_start()), self.source.clone())
    }

    /// Range of the close portion.
    pub fn close_range(&self) -> SourceRange {
        SourceRange::with_source(Some(self.inner_end()), self.end, self.source.clone())
    }

    /// Range of the inner portion (between the open and close tags).
    pub fn inner_range(&self) -> SourceRange {
        SourceRange::with_source(
            Some(self.inner_start()),
            Some(self.inner_end()),
            self.source.clone(),
        )
    }

    /// Range of the outer portion.
    pub fn outer_range(&self) -> SourceRange {
        SourceRange::with_source(self.start, self.end, self.source.clone())
    }

    /// Shift all offsets by `amount`.
    pub fn offset(&self, amount: isize) -> DomSourceRange {
        let shift = |o: Option<usize>| o.and_then(|v| v.checked_add_signed(amount));
        DomSourceRange {
            start: shift(self.start),
            end: shift(self.end),
            source: self.source.clone(),
            open_width: self.open_width,
            close_width: self.close_width,
            leading_ws: self.leading_ws,
            trailing_ws: self.trailing_ws,
        }
    }

    /// Whether both tag widths are non-null and non-negative.
    pub fn has_valid_tag_widths(&self) -> bool {
        self.open_width.is_some() && self.close_width.is_some()
    }

    /// Whether either trimmed-whitespace width is non-zero.
    pub fn has_trimmed_ws(&self) -> bool {
        self.leading_ws != 0 || self.trailing_ws != 0
    }

    /// Whether the leading-whitespace width is valid (`!= -1`).
    pub fn has_valid_leading_ws(&self) -> bool {
        self.leading_ws != -1
    }

    /// Whether the trailing-whitespace width is valid (`!= -1`).
    pub fn has_valid_trailing_ws(&self) -> bool {
        self.trailing_ws != -1
    }

    /// Convert a plain [`SourceRange`] to a zero-width-container DSR.
    pub fn from_tsr(tsr: &SourceRange) -> DomSourceRange {
        DomSourceRange {
            start: tsr.start,
            end: tsr.end,
            source: tsr.source.clone(),
            ..Default::default()
        }
    }

    /// Serialize to the `data-parsoid` `dsr` array (`[start, end, openWidth,
    /// closeWidth]`, plus `leadingWS`/`trailingWS` when non-zero).
    pub fn to_json_array(&self) -> Vec<Option<usize>> {
        let mut a = vec![self.start, self.end, self.open_width, self.close_width];
        if self.leading_ws != 0 || self.trailing_ws != 0 {
            a.push(Some(self.leading_ws as usize));
            a.push(Some(self.trailing_ws as usize));
        }
        a
    }
}

/// Data that's necessary for selective updates (whether html→wt or wt→html).
/// Faithful port of PHP's `Core\SelectiveUpdateData`. This is always the
/// revision (current or previous) wikitext & html.
///
/// Only `rev_text` is modeled for now: it is the sole field the selser
/// serializer reads (`getOrigSrc`/`isValidDSR`). `rev_html`/`rev_dom`/
/// `template_title`/`mode` are carried through when provided, but the
/// `revDOM` document graph is a type this codebase does not yet materialize
/// for the selective-update path (see `selser.rs` porting note).
#[derive(Debug, Clone, Default)]
pub struct SelectiveUpdateData {
    /// The revision wikitext source.
    pub rev_text: String,
    /// The revision HTML (when available).
    pub rev_html: Option<String>,
    /// If doing a selective update for a template edit, the edited template's
    /// title string.
    pub template_title: Option<String>,
    /// Options for selective HTML updates: template, section, generic.
    pub mode: Option<String>,
}

impl SelectiveUpdateData {
    pub fn new(rev_text: impl Into<String>) -> Self {
        Self {
            rev_text: rev_text.into(),
            rev_html: None,
            template_title: None,
            mode: None,
        }
    }
}

/// Basic check if a DOM Source Range (DSR) is valid (faithful to
/// `Utils::isValidDSR`).
///
/// Only checks for underflow (null / negative offsets), not overflow, and
/// does not verify `start <= end` nor `openWidth + closeWidth <= end - start`;
/// those checks live in `SerializerState::isValidDSR`.
///
/// When `all` is true, the container tag widths must also be valid
/// (non-null, non-negative).
pub fn is_valid_dsr(dsr: Option<&DomSourceRange>, all: bool) -> bool {
    let is_valid_offset = |n: Option<usize>| n.is_some();
    match dsr {
        None => false,
        Some(dsr) => {
            is_valid_offset(dsr.start)
                && is_valid_offset(dsr.end)
                && (!all || (is_valid_offset(dsr.open_width) && is_valid_offset(dsr.close_width)))
        }
    }
}

impl From<crate::wikitext::tokens_v2::DomSourceRange> for DomSourceRange {
    /// Lift the tokenizer-side DSR into the serializer-facing DSR, defaulting
    /// the html2wt-only fields that the tokenizer does not model (trimmed-WS
    /// widths, and the `source` text — which `getOrigSrc` supplies at call time).
    fn from(tsr: crate::wikitext::tokens_v2::DomSourceRange) -> Self {
        DomSourceRange {
            start: tsr.start,
            end: tsr.end,
            source: None,
            open_width: tsr.open_width,
            close_width: tsr.close_width,
            leading_ws: 0,
            trailing_ws: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inner_range_arithmetic() {
        let dsr = DomSourceRange::new(Some(10), Some(30), Some(2), Some(3), 0, 0);
        assert_eq!(dsr.inner_start(), 12);
        assert_eq!(dsr.inner_end(), 27);
        assert_eq!(dsr.inner_length(), 15);
        let inner = dsr.inner_range();
        assert_eq!(inner.start, Some(12));
        assert_eq!(inner.end, Some(27));
    }

    #[test]
    fn test_open_close_range() {
        let dsr = DomSourceRange::new(Some(10), Some(30), Some(2), Some(3), 0, 0);
        assert_eq!(dsr.open_range().end, Some(12));
        assert_eq!(dsr.close_range().start, Some(27));
        assert_eq!(dsr.close_range().end, Some(30));
    }

    #[test]
    fn test_has_trimmed_ws() {
        let plain = DomSourceRange::new(Some(0), Some(5), None, None, 0, 0);
        assert!(!plain.has_trimmed_ws());
        let trimmed = DomSourceRange::new(Some(0), Some(5), None, None, 2, -1);
        assert!(trimmed.has_trimmed_ws());
        assert!(trimmed.has_valid_leading_ws());
        assert!(!trimmed.has_valid_trailing_ws());
    }

    #[test]
    fn test_offset() {
        let dsr = DomSourceRange::new(Some(10), Some(20), Some(2), Some(3), 0, 0);
        let shifted = dsr.offset(3);
        assert_eq!(shifted.start, Some(13));
        assert_eq!(shifted.end, Some(23));
    }

    #[test]
    fn test_null_dsr_length() {
        let sr = SourceRange::new(None, None);
        assert_eq!(sr.length(), 0);
        assert_eq!(sr.substr("hello"), "");
    }
}
