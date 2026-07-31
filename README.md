# Rustoid

A Rust reimplementation of the Wikimedia [Parsoid](https://www.mediawiki.org/wiki/Parsoid) parser — bidirectional wikitext ↔ HTML5 conversion with byte-perfect output compatibility.

## Status

🚧 **All phases have initial implementations** — 130 unit tests pass, 7/144 Parsoid fixture tests pass (5%).

Active work: deeper architectural changes for block HTML tags, pre-mode tokenization, bold/italic nesting, and comment handling.

| Phase | Status |
|-------|--------|
| 0 — Project setup | ✅ Done |
| 1 — Core types & traits | ✅ Done |
| 2 — Wikitext tokenizer | ✅ Done (pre-mode support added) |
| 3 — Template expander | ✅ Done (iterative work-stack preprocessor) |
| 4 — Lua/Scribunto engine | ✅ Done |
| 5 — AST / Tree builder | ✅ Done (block HTML content collection) |
| 6 — HTML serialization | ✅ Done |
| 7 — HTML→wikitext round-trip | ✅ Done |
| 8 — Selective serialization | ✅ Done |
| 9 — Data source impls | ✅ Done |
| 10 — Test harness | ✅ Done (11 Parsoid fixture files) |
| 11 — Test pass | 🔄 7/144 Parsoid fixtures (5%) |
| 12 — CLI binary | 🔄 In progress |
| 13 — Polish | ⬜ Pending |

See [PLAN.md](PLAN.md) for the full roadmap.

## Quick start

```bash
cargo build --workspace
cargo test --workspace
cargo run -- render --page "Main Page"
```

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).
