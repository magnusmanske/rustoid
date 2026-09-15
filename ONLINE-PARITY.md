# Online-parity goal

This document records the **current primary goal of the codebase** and the plan to
reach it. `REMAINING-BUCKETS.md` is the per-fixture log for the *existing* target;
this file is about the next one.

## The goal

A test binary that, given a **wiki** and a **page title**:

1. fetches the wiki's own (online) Parsoid HTML for the page,
2. fetches that page's wikitext from the same wiki, **cached**,
3. runs the rustoid parser over that wikitext,
4. **compares** the two results, aiming for *exact* equality.

Stated as a target: for a page on a live wiki, rustoid's output should be
indistinguishable from the HTML that wiki's Parsoid produced for it.

Reaching that requires reimplementing all on-wiki extensions, prominently the
**Lua/Scribunto** plugin.

## Why this is a *different* target than the fixture suite

This is the most important thing to understand before starting, and it reframes
the work:

- The 891-fixture suite (`rustoid-core/tests/fixtures/`) exercises Parsoid in
  **standalone** mode. That is what the current 871/891 measures.
- A live wiki serves Parsoid in **integrated** mode. MediaWiki core runs the
  preprocessor and the extensions; Parsoid only renders the resulting token
  stream.

Consequences:

- **Standalone Parsoid does not implement `#invoke` at all.** `baseconfig/enwiki.json`
  lists zero `functionhooks`, and there is not a single `#invoke` in Parsoid's
  own `tests/parser/*.txt`. For an unimplemented parser function standalone
  Parsoid emits the literal error
  `Parser function implementation for pf_<name> missing in Parsoid.`
  (`src/Wt2Html/TT/TemplateHandler.php:580`). So Lua is invisible to the fixture
  suite *by construction* — 871/891 says nothing about it.
- Parsoid ships only *built-in* extension handlers (`src/Ext/`: `Gallery`,
  `Nowiki`, `Pre`, `JSON`, `Indicator`). `Cite` (`<ref>`) and `Scribunto` live in
  MediaWiki core / extension repos and are only present in integrated mode.
- Integrated mode also uses **MW core's preprocessor**, whose edge cases differ
  from Parsoid's standalone one. That is exactly why this repo already documents
  a family of fixtures as `+integrated`-only and "unreachable" — those
  expectations are the integrated-mode results.

So online parity is a **superset** of the current target: it needs everything
already working (tokenizer, tree builder, serializer — shared by both modes)
*plus* MW-core preprocessor fidelity, *plus* the wiki's extensions, *plus* Lua.

The good news: the 871/891 core is re-used as-is. This is additive work, not a
rewrite.

### What "exact equality" cannot mean

Some output is time- and state-dependent: `{{CURRENTYEAR}}`, `{{#time:}}`,
`{{age in days}}`, `{{NUMBEROFARTICLES}}`, and any Lua module calling
`os.date`/`mw.language:formatDate`/`mw.site.stats`. The wiki's Parsoid baked in
*its* clock and counters; a replay cannot reproduce those without being told
them. The harness must therefore record the wall-clock time and the relevant
site statistics alongside the fetched HTML, and rustoid must be able to *replay*
them. Pages that cannot be made deterministic are excluded from the byte-exact
score and tracked separately.

## Current inventory (verified, not assumed)

| Piece | State |
|---|---|
| Parser core (wikitext → HTML) | **Working** — 871/891 fixtures, 679 lib tests |
| CLI (`rustoid-cli`) | **Stubs only.** `render`/`roundtrip`/`test`/`serve` all print "not yet implemented" |
| Lua engine (`lua/engine.rs`, 666 lines) | Present: sandbox, 8 `mw` sub-tables, 17 functions, 16 unit tests. **Not wired into the parser** — no `#invoke` dispatch anywhere |
| Extension API (`traits.rs::ExtensionHandler`) | Trait exists; **zero implementations**. Returns `String`, which is too coarse for `mw:Extension/<name>` + `data-mw`/`data-parsoid` |
| Extension tags | Handled *generically*; `nowiki`/`pre`/gallery/i18n special-cased. No Cite, no Scribunto |
| Site config | **Hardcoded** `MockSiteConfig` (enwiki-shaped). No way to load a wiki's real config |
| API data source (`mw_api.rs`) | Exists with in-memory TTL cache: page/template/module/file/redirect. Not enabled by the CLI (feature off) |
| Persistent cache | **None** — in-memory only |
| Comparison harness | **Does not exist** |
| Corpus / golden files | **Does not exist** |

## Verified API surface (for the harness)

All confirmed live, with `curl`, against `en.wikipedia.org`:

| Purpose | Endpoint | Status |
|---|---|---|
| Parsoid HTML, latest | `GET /w/rest.php/v1/page/{title}/html` | 200 |
| Parsoid HTML, **revision-pinned** | `GET /api/rest_v1/page/html/{title}/{revision}` | 200 |
| Wikitext at a revision | `GET /w/rest.php/v1/revision/{revid}` → `.source` | 200 |
| Latest revid for a title | `GET /w/api.php?action=query&prop=revisions&rvprop=ids` | 200 |
| Site config | `GET /w/api.php?action=query&meta=siteinfo&siprop=namespaces\|namespacealiases\|magicwords\|functionhooks\|extensiontags\|interwikimap\|general` | 200 |

Two operational facts worth recording now:

- **Rate limiting is real and immediate.** A handful of rapid requests already
  returned `You are making too many requests to the API`. Every fetch needs a
  descriptive `User-Agent`, serialisation with a delay, and aggressive caching.
- **Revision pinning matters.** Fetch the revid first, then fetch both the
  wikitext and the Parsoid HTML *at that revid*; otherwise an intervening edit
  makes the comparison meaningless.

## Plan

Ordered so that measurement comes first: without a scoreboard, the extension and
Lua work is unverifiable.

### Phase 0 — The measurement instrument

`rustoid compare` in the CLI, plus a batch mode.

- `--wiki <host>` / `--page <title>` / `--revision <id>` (default: latest),
  `--corpus <file>` for batches, `--cache <dir>`, `--offline`.
- Fetch revid → wikitext → Parsoid HTML, all pinned; record the timestamp and
  site stats from the same moment for replay.
- **Persistent disk cache** keyed by `(wiki, title, revid)`, with a manifest so
  runs are reproducible offline and re-runs never re-hit the wiki.
- Normalise both sides the way Parsoid's own `Test::normalizeHTML` does (DOM
  normalisation; optionally strip `data-parsoid`/`data-mw`), then diff, and
  report the *first* difference plus a categorised reason.
- Enable the `mwapi` feature for the CLI (currently off).
- **Exit criterion:** 10 hand-picked pages produce stable, reproducible results
  from cache alone, with `--offline`.

### Phase 1 — Site config from the wiki

- Load `SiteConfig` from `siteinfo` (namespaces + aliases, magic words, function
  hooks, extension tags, interwiki map, general) instead of a hardcoded list, and
  persist it with the cache.
- Ingest Parsoid's `baseconfig/*.json` too, for offline fixture-grade runs.
- **Exit criterion:** the extension-tag list and namespace table used for a run
  demonstrably come from the wiki, not `mock.rs`.

### Phase 2 — Corpus and scoreboard

- A curated corpus spanning the failure space: plain prose, tables, infoboxes,
  references, module-heavy, magic-word-heavy, edge cases — plus a random sample
  for breadth.
- Golden Parsoid HTML in-repo so regressions are catchable without network.
- **Exit criterion:** one command prints a score and a diff-category histogram.

### Phase 3 — `#invoke` end to end (the biggest single lever)

- Wire `invoke` into the parser-function dispatch and build the module loader
  (`Module:` namespace, `require`, `package.loaders`, `mw.loadData`).
- Frame API, in this order: `frame.args`, `getParent`, `getTitle`,
  **`expandTemplate`**, **`callParserFunction`**, `preprocess`, `newChild`,
  `argumentPairs`.
- **Strip markers.** Scribunto protects non-wikitext output with `\127'"`UNIQ…`
  markers that must survive the round trip back through the parser. Getting this
  wrong produces subtly mangled output everywhere, so it belongs in this phase.
- **Exit criterion:** a page whose infobox is module-driven matches byte-exactly.

### Phase 4 — Lua API breadth

- `mw.ustring` complete (`gsub`/`gmatch`/`find`/`match`/`sub`/`rep`/…), then
  `mw.title` completeness, `mw.site`, `mw.message` with real i18n lookup,
  `mw.text`, `mw.html`, `mw.language`, `mw.uri`.
- `mw.title:getContent`/`exists`/`redirectTarget` via `DataSource`;
  `mw.dumpObject`, `mw.log`, `mw.addWarning`, `mw.getCurrentFrame`.
- Instruction-count and memory limits enforced, with a wall-clock timeout.
- **Exit criterion:** the score on the module-heavy corpus moves materially.

### Phase 5 — On-wiki extensions

- **Cite first** (`<ref>`, `<references>`): names and groups, back-links, the
  `mw:Extension/ref` wrapper with its `data-mw` body, and the `<references>`
  list assembly. This is the most-used extension after Lua.
- Then Poem, finishing Gallery, SyntaxHighlight/Source, Math, Templatedata,
  Templatestyles; then the placeholder-semantics ones (Timeline, Graph,
  Mapframe).
- This needs the extension API widened from `-> Result<String>` to a token/DOM
  level so handlers can emit `mw:Extension/<name>` with `data-mw`/`data-parsoid`,
  and so they can be *hybrid* (part wikitext, part HTML) as Parsoid's are.
- **Exit criterion:** pages with references match.

### Phase 6 — Integrated-mode semantics

- Close the gap to MW core's preprocessor where it differs from Parsoid
  standalone — the cause of the `+integrated`-only divergences already logged in
  `REMAINING-BUCKETS.md`.
- **Exit criterion:** the previously-"unreachable" fixture family becomes
  reachable and passing.

### Phase 7 — Scale and hardening

- Large pages, deep transclusion, resource limits, parallelism, cache
  management, resumable batches, and a perf budget.

## Risks

- **Scribunto fidelity is open-ended.** `Module:Citation/CS1` alone is thousands
  of lines of Lua. Expect long tail; measure by corpus score, not by "done".
- **Determinism.** See "What exact equality cannot mean" above. The fix (record
  and replay time/stats) must land in Phase 0, or every score is noisy.
- **Rate limits and ToS.** Polite fetching, caching, and revision pinning are
  correctness requirements, not polish.
- **Integrated-mode preprocessor differences are subtle and un(der)documented.**
  They will be found by differential testing, not by reading.
- **The extension API's current shape is too narrow** and will need widening
  before Cite can be faithful; budget for that in Phase 5 rather than assuming it
  is a small patch.

## How progress is measured

Per wiki and corpus: pages matching byte-exactly, plus a histogram of first-
difference categories. The fixture score (currently 871/891) remains the guard
for the shared core, so this work must not regress it.
