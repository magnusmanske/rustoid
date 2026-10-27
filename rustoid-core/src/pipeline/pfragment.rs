//! The parse's pFragment counter.
//!
//! A `data-mw` body records a piece that has no wikitext form as
//! `{{#parsoid\0fragment:N}}` (`PipelineUtils::pFragmentToParsoidFragmentMarkers`).
//! `N` is **not** a DOM fragment id: Parsoid stamps it from a counter that it
//! bumps once per atomic piece of every fragment it prepares — a template
//! expansion's preprocessed source, a `#tag:<extension>` call's own fragment and
//! its body, a module's output. The number is document-wide and grows into the
//! hundreds on a large page; it also moves when an unrelated part of the page
//! changes.
//!
//! rustoid spelled a placeholder's `data-fragment-id` instead, which is a
//! different (and much smaller) sequence; the two agree only by accident.
//!
//! The counter lives outside [`Parser`](crate::pipeline::parser::Parser) because
//! `#tag` is a static parser function ([`ParserFunctions::pf_tag`]) and building
//! its source is what consumes the numbers. A page's parse runs on one thread
//! (each render owns a current-thread runtime), so a thread-local needs no lock;
//! it is cleared when the parse starts.
//!
//! [`ParserFunctions::pf_tag`]: crate::pipeline::parser_functions::ParserFunctions::pf_tag

use std::cell::Cell;

thread_local! {
    static NEXT: Cell<usize> = const { Cell::new(0) };
    /// Set while a `#tag:<extension>` call's arguments expand. A parser function
    /// reached from there is evaluated lazily as an atomic piece of that
    /// argument, so it spends no number of its own.
    static LAZY: Cell<bool> = const { Cell::new(false) };
}

/// Start a parse with the counter clear.
pub(crate) fn reset() {
    NEXT.with(|n| n.set(0));
    LAZY.with(|l| l.set(false));
}

/// Take the next number for an atomic piece.
pub(crate) fn allocate() -> usize {
    NEXT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

/// Take a run of numbers for `count` atomic pieces of one prepared fragment.
pub(crate) fn allocate_many(count: usize) {
    if count > 0 {
        NEXT.with(|n| n.set(n.get() + count));
    }
}

/// Take a number for a `#tag:<extension>` call's own fragment, unless the call
/// is being evaluated lazily inside another such call's argument.
pub(crate) fn allocate_tag() {
    if !in_lazy_body() {
        allocate();
    }
}

/// Mark the arguments being expanded as a `#tag:<extension>` call's, returning
/// the previous state for [`restore_lazy`].
pub(crate) fn set_lazy(lazy: bool) -> bool {
    LAZY.with(|l| l.replace(lazy))
}

/// Restore the state [`set_lazy`] returned.
pub(crate) fn restore_lazy(previous: bool) {
    LAZY.with(|l| l.set(previous));
}

fn in_lazy_body() -> bool {
    LAZY.with(|l| l.get())
}
