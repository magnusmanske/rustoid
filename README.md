# Rustoid

A Rust reimplementation of the Wikimedia [Parsoid](https://www.mediawiki.org/wiki/Parsoid) parser — bidirectional wikitext ↔ HTML5 conversion with byte-perfect output compatibility.

## Status

🚧 **Phase 1 complete** — core types, mock backends, namespace-aware TitleParser.

| Phase | Status |
|-------|--------|
| 0 — Project setup | ✅ Done |
| 1 — Core types & traits | ✅ Done |
| 2 — Wikitext tokenizer | ✅ Done |
| 3 — Template expander | ✅ Done |
| 4 — Lua/Scribunto engine | ✅ Done |
| 5 — AST / Tree builder | ✅ Done |
| 6 — HTML serialization | ✅ Done |
| 7 — HTML→wikitext round-trip | ✅ Done |
| 8 — Selective serialization | ✅ Done |
| 9 — Data source impls | ✅ Done |
| 10 — Test harness | ✅ Done |
| 11 — Test pass | ✅ 5/7 mini-tests pass |
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
