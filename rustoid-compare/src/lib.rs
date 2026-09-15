//! `rustoid-compare` — compare rustoid's output against a live wiki's Parsoid.
//!
//! The goal is *online parity*: given a wiki and a page title, fetch that wiki's
//! own Parsoid HTML and the page's wikitext, run rustoid over the wikitext, and
//! compare. See `ONLINE-PARITY.md` for the plan.
//!
//! This is a separate crate rather than a `rustoid-cli` subcommand so it can be a
//! dev-dependency, is not shipped in the `rustoid` binary, and can pull in
//! network/HTTP dependencies without infecting the parser.
//!
//! Module layout:
//!
//! - [`cache`] — persistent, flushable, per-wiki wikitext cache. All fetching
//!   goes through it, both to stay within Wikimedia's rate limits and to make
//!   runs reproducible offline.
//! - [`wire`] — thin, revision-pinned client for the MediaWiki REST/Action API.
//! - [`harness`] — drives the two sides and reports the comparison.

//! - [`siteconfig`] — the wiki's own `SiteConfig`, parsed from its `siteinfo`,
//!   so an online comparison is not silently capped by a hardcoded mock.
//! - [`corpus`] — the set of pages to compare, and the tag vocabulary that lets
//!   a failure be attributed to a feature area rather than merely counted.
//! - [`scoreboard`] — tallies a corpus run into a score and two histograms.

pub mod cache;
pub mod corpus;
pub mod error;
pub mod harness;
pub mod scoreboard;
pub mod siteconfig;
pub mod wire;

pub use cache::{CachedBody, EntryKind, EntryMeta, WikiCache};
pub use corpus::{Corpus, CorpusEntry};
pub use error::{CompareError, Result};
pub use harness::{
    CachedDataSource, CompareRequest, Comparison, Outcome, compare_html, compare_page,
    load_site_config,
};
pub use scoreboard::{Row, Scoreboard};
pub use siteconfig::WikiSiteConfig;
pub use wire::{Wiki, WikiClient};
