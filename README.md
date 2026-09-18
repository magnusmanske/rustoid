# Rustoid

A Rust reimplementation of the Wikimedia [Parsoid](https://www.mediawiki.org/wiki/Parsoid) parser — bidirectional wikitext ↔ HTML5 conversion, targeting **identical output** to Parsoid.

## Current goal: parity with *online* Parsoid

**This is the active goal of the codebase.** See [ONLINE-PARITY.md](ONLINE-PARITY.md).

A test binary that takes a **wiki** and a **page title**, fetches that wiki's own (online) Parsoid HTML, fetches the page's wikitext (cached) from the same wiki, runs rustoid over it, and compares the two — aiming for *exact* equality. This requires reimplementing all on-wiki extensions, prominently the **Lua/Scribunto** plugin.

This is a *different* target from the fixture suite below. The fixtures exercise
Parsoid in **standalone** mode; a live wiki serves **integrated** mode, where
MediaWiki core runs the preprocessor and the extensions. Standalone Parsoid does
not implement `#invoke` at all, and ships only a handful of built-in extensions —
so the fixture suite cannot measure Lua or Cite.

## Status

| Area | Status |
|------|--------|
| Parser core (wikitext → HTML) | **876/896 fixtures (98%)**, 769 lib tests |
| HTML → wikitext (round-trip) | Working |
| Selective serialization (selser) | Working |
| Template expansion | Working |
| Lua/Scribunto | Engine wired to `#invoke`, with `frame:expandTemplate`/`callParserFunction`/`preprocess`; `mw` surface still incomplete |
| On-wiki extensions (Cite, …) | Not implemented (built-ins + Templatestyles only) |
| Online-parity harness (`rustoid-compare`) | Working — revid-pinned fetch, per-wiki cache, offline replay, corpus scoreboard. Online score: **0 pages byte-exact** |
| CLI (`rustoid-cli`) | Subcommands are stubs |

The phases in [PLAN.md](PLAN.md) (0–13) are the original, now largely complete
plan for the standalone parser. Progress on the current goal is tracked in
[ONLINE-PARITY.md](ONLINE-PARITY.md); per-fixture detail lives in
[REMAINING-BUCKETS.md](REMAINING-BUCKETS.md).

## Quick start

```bash
cargo build --workspace
cargo test --workspace

# Fixture suite (the standalone-mode score)
cargo test -p rustoid-core --test integration_test -- --nocapture
```

## Repository layout

```
rustoid/
├── PLAN.md                # roadmap (standalone parser phases)
├── ONLINE-PARITY.md       # current goal + plan (the active workstream)
├── REMAINING-BUCKETS.md   # per-fixture notes for the fixture suite
├── rustoid-core/          # parser, tokenizer, tree builder, serializers, Lua
├── rustoid-cli/           # command-line binary
└── enwiki-parser-extensions.md
```

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).
