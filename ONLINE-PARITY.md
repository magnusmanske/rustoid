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
| **Comparison harness** (`rustoid-compare`) | **Working** — revid-pinned fetch, persistent per-wiki cache, offline replay, manifest recovery (`--reindex`), first-difference reporting. 84 tests |
| CLI (`rustoid-cli`) | **Stubs only.** `render`/`roundtrip`/`test`/`serve` all print "not yet implemented" |
| Lua engine (`lua/engine.rs`, 666 lines) | Present: sandbox, 8 `mw` sub-tables, 17 functions, 16 unit tests. **Not wired into the parser** — no `#invoke` dispatch anywhere |
| Extension API (`traits.rs::ExtensionHandler`) | Trait exists; **zero implementations**. Returns `String`, which is too coarse for `mw:Extension/<name>` + `data-mw`/`data-parsoid` |
| Extension tags | Handled *generically*; `nowiki`/`pre`/gallery/i18n special-cased. No Cite, no Scribunto |
| Site config | **Hardcoded** `MockSiteConfig` (enwiki-shaped). No way to load a wiki's real config |
| API data source (`mw_api.rs`) | Exists with in-memory TTL cache. Superseded for this purpose by the harness's cache-backed `DataSource` |
| Persistent cache | **Done** — per-wiki, flushable, offline-capable |
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

**Status: landed** as a separate `rustoid-compare` crate (chosen over a
`rustoid-cli` subcommand so it can be a dev-dependency, stays out of the shipped
`rustoid` binary, and keeps the network dependencies away from the parser).

- [x] Separate `rustoid-compare` crate + `rustoid-compare` binary.
- [x] Revision-pinned client ([`wire`](../rustoid-compare/src/wire.rs)): resolves
      the latest revid, then fetches the wikitext and the wiki's Parsoid HTML *at
      that revid*, so an intervening edit cannot invalidate a comparison.
- [x] **Persistent, flushable, per-wiki wikitext cache**
      ([`cache`](../rustoid-compare/src/cache.rs)): one directory per wiki host
      (`<root>/<host>/index.json` + `pages/<key>.txt`). Bodies are separate files
      so a large cache stays inspectable; the manifest is written write-then-rename
      so a crash cannot truncate it. Keys are sanitised so a wiki-supplied title
      cannot escape the directory. `--flush` drops one wiki, `--flush-all`
      everything.
- [x] Cache-backed `DataSource`, so template and module fetches *during
      expansion* are persisted per title too — verified: one page produced 85
      cached entries.
- [x] `--offline`: a miss is reported as skipped rather than fetched, so a run is
      reproducible and rate-limit-free. Verified: an offline replay of the same
      page returns the identical revid and the identical diff.
- [x] Outcome categorisation (`match` / `differ:<kind>` / `skipped`) for the
      scoreboard histogram.

Verified end-to-end against the live wiki:

```
$ rustoid-compare --wiki en.wikipedia.org --page "UFC BJJ"
UFC BJJ @ r1371233379 — DIFFER
first difference at byte 1:
  parsoid: "<section data-mw-section-id=\"0\" id=\"mwAQ\"><div class=\"shortde"
  rustoid: "<p about=\"#mwt30\" typeof=\"mw:Transclusion\" data-parsoid='{\"pi"
  parsoid 239179 bytes, rustoid 1112120 bytes
```

That size gap is the expected shape of what remains: missing Lua modules and
on-wiki extensions. The instrument itself is what Phase 0 needed to produce.

Still to come in this phase: corpus/batch mode (a list of titles), golden files,
and recording the run's wall-clock time and site stats for replay (see
"What exact equality cannot mean").

#### Recovering a cache whose manifest was lost

A run can be interrupted after bodies are written but before `index.json` is. The
bodies are the expensive, rate-limited part; the manifest is metadata. But `get`
consults the manifest first, so that state is worse than useless — a cache holding
thousands of downloaded templates reads as **empty**, and every offline page
reports `skipped`, which looks like a network or coverage problem rather than a
lost index. It happened: a populated enwiki cache was left holding 60 bodies and
no index, and `--offline` failed at the first step with
`siteinfo not cached and --offline was requested`.

`--reindex` rebuilds the manifest from the body files. Two things make it work:

- **The kind lives in the key**, and the filename carries the key, so
  `mod:Module:Hatnote list` is recoverable exactly rather than guessed. The old
  filename scheme escaped every `:` as `__`; those bodies are still readable, and
  the *recorded* title is what `get` looks the body up by, so a lossy name does
  not make an entry unreachable.
- **A revision is not recoverable from a body** — it only ever lived in the
  manifest. That would still sink an offline run, so a recovered cache falls back
  to the revision that the cached Parsoid HTML states about itself
  (`Special:Redirect/revision/<id>` on the root element). This is the second use
  of that stamp; pinning by it is what makes a reindexed cache comparable at all,
  rather than merely non-empty.

So `--reindex` + `--offline` reproduces a comparison with no manifest and no
network. What it cannot restore is `fetched_at`, and it cannot distinguish a
stale body from a fresh one — it is a recovery tool, not a substitute for the
manifest. The lesson is that the manifest is the only file whose loss is
*unrecoverable*, which is what keeps it small and write-then-rename.

### Phase 1 — Site config from the wiki — *done*

- Load `SiteConfig` from `siteinfo` (namespaces + aliases, magic words, function
  hooks, extension tags, interwiki map, general) instead of a hardcoded list, and
  persist it with the cache. — **done** (`rustoid-compare/src/siteconfig.rs`)
- **Exit criterion:** the extension-tag list and namespace table used for a run
  demonstrably come from the wiki, not `mock.rs`. — **met**: `rustoid-compare`
  no longer references `MockSiteConfig`, and a live enwiki run reports
  `28 extension tags, 112 function hooks, 285 magic words, 30 namespaces, 781
  interwiki`, matching a direct `siteinfo` query.

Two non-obvious `siteinfo` wire traps, both now covered by tests:

- The localized namespace name is spelled `*` in formatversion=1 and `name` in
  formatversion=2, so both must be understood.
- `serde`'s `rename = "*"` **silently deserializes to `None`** — it does not
  error. Anything reading a `*` key has to capture the object and index it.

Still to come in this phase: ingesting Parsoid's `baseconfig/*.json` for
fixture-grade offline runs (the parser's own fixture suite already pins its own
configuration and is unaffected).

### Phase 2 — Corpus and scoreboard — *done*

- A curated corpus spanning the failure space: plain prose, tables, infoboxes,
  references, module-heavy, magic-word-heavy, edge cases — plus a random sample
  for breadth. — **done** (`rustoid-compare/corpus/enwiki.txt`, 49 entries,
  compiled in via `include_str!`; `--corpus <file>` takes a custom one).
  Entries are tagged with the feature areas they exercise, verified by
  inspecting each page's wikitext rather than assumed.
- **Exit criterion:** one command prints a score and a diff-category histogram. —
  **met**: `--corpus default` prints a score, a by-outcome histogram, a by-tag
  table, a failure list, and (with `-v`) every first difference.

Two decisions worth recording, both of which changed the numbers' meaning:

- **Two histograms, not one.** By *category* says what the parser did wrong; by
  *tag* says which body of work that maps onto and whether anything passes at
  all. Neither alone is enough: a page yields a single category but carries
  several tags, and a tag whose pages all fail identically is invisible in a
  category tally.
- **Skips are excluded** from the score and the tag table. A page that could not
  be fetched says nothing about parser parity, and counting it as a failure
  would fill the work list with entries that have nothing wrong with them. This
  is what makes an `--offline` run comparable to an online one.

The classification also had to change to be worth anything: it previously
sniffed a 60-byte window around the first differing *byte*, which reflects
position rather than cause — a `<td>` that identifies a table problem is often
several hundred bytes before the byte that actually differs. It now sees 400
bytes on each side, and buckets are ordered most-specific-first so a page whose
only real problem is Lua is not reported as an extension difference.

Building the corpus immediately exposed a **cache correctness bug** that had
been invisible until a template-heavy page was tried. The data source handed to
the parser opened its *own* `WikiCache`, so it held a separate in-memory
manifest from the harness's; the two overwrote each other's `index.json`. Bodies
still landed on disk, so nothing failed — but after a full cold fetch of
`Cristiano Ronaldo` there were **445 template bodies on disk and 3 manifest
entries**, so nearly every template was re-downloaded on every run. A dense page
took 7m28s and only 3 entries survived.

The fix is one shared `Arc<Mutex<WikiCache>>`. While fixing it, `put` was also
decoupled from `write_index`, because re-serialising the whole manifest on each
of hundreds of fetches is quadratic; the manifest is now written every 64
entries and at the end of a run. This matters for what comes next: Lua support
will fetch *more* templates per page, not fewer, so a harness that got slower
with every capability added would defeat the purpose of having one.

Still to come in this phase: golden Parsoid HTML committed in-repo, so
regressions are catchable without network. The cache carries the same
information today, but it is a local artifact rather than a reviewable one.

#### A tokenizer bug the corpus found, and a second one it did not

The first corpus run never finished. `RUSTOID_TRACE_FETCH=1` showed the page
`Israel` requesting `Template:Def` — a template that **does not exist** and
appears in no input — thousands of times. Root cause, in `find_template_closing`
(`rustoid-core/src/wikitext/tokenizer_v2.rs`): a `{{{…}}}` argument reference was
treated as pushing a `}}` closer, so the enclosing template closed one brace
early.

```text
{{y|def={{{def|no}}}}}    inner was "y|def={{{def|no}}", dropping a brace
                          → the leftover re-read as {{def|no}}
                          → a bogus transclusion of Template:Def
```

PHP is explicit that the closer for an argument reference is `}}}`: its
`preproc_piece` scans tplarg contents with `preproc_stop="}}}"` and admits a `}`
only when it is not followed by `}}` (`&<preproc_stop=="}}}"> !"}}"`).

The fix has **two** halves, and skipping either one is wrong:

- Treat a `{{{…}}}` as closed by `}}}` — otherwise the enclosing template is
truncated (above).
- Reject a `{{{` that is not a well-formed argument reference, falling back to
  reading the `{` as ordinary content — otherwise `{{{!}}` breaks. `{{{!}}` is
  not an argument reference at all; it is `{` + `{{!}}`, the idiom that renders
  `{|` and opens a table, and it has no `}}}`.

Half of the fix alone passes the corpus reproduction but drops the fixture suite
from 871 to 869 (`Template pre: Table`). Both halves together: 871/891
unchanged, and `Template:Def` requests go from 1813 to **zero**.

This is also why the fixture suite never caught it. The bogus token lives in an
argument value, so it only expands if the template actually *uses* that
parameter. Fixture templates are small and ignore their last argument; a real
infobox uses every one. `{{T|p={{{x|default}}}}}` and `{{T|p={{{x|}}}}}`
behaved differently — only the first leaves a well-formed `{{x|default}}`
behind — which no fixture distinguished.

**Still open.** With that fixed, a warm-cache online run of `Israel` *still* does
not terminate: ~198000 requests against 9 network fetches. It is not a cycle in
`Template:Def`'s sense — the longest run of identical consecutive requests is
24 — but a runaway expansion tree, most likely the exponential blowup that
MediaWiki bounds with its preprocessor node-count limit, which rustoid does not
implement. `#ifexist` and `#invoke` are both unimplemented, and the offending
templates (`Template:Country topics`) lean on both, so this probably belongs
with the Phase 3 `#invoke` work rather than here.

#### The expansion blowup, measured — and a bad hour spent misreading it

`Zebra` reproduces this, so it can now be worked on with the corpus cache rather
than only on `Israel`. It is **not** a deadlock and **not** a parser bug, and both
of those were believed for a while, which is worth recording.

What it actually is: the expansion genuinely runs, but far slower than it should.
Reaching the reference list needs a taxobox expansion whose template chain
(`Template:Is italic taxon` → `Taxobox colour` → `Delink` → `Taxonomy/…` →
`Taxobox colour` → `Delink` → …) keeps re-entering the same templates many times
over. The process sits at **99% CPU with a growing RSS**, which is the shape of an
exponential blowup: each round does real work, so no bound is hit and nothing
looks stuck. MediaWiki avoids this with a preprocessor node-count limit
(`$wgMaxPPNodeCount`) and an expansion-depth limit; rustoid has neither, and this
is the case that needs both.

**The measurement mistake, because it cost more than the bug did.** The obvious
diagnostic — "is it working, or wedged?" — was answered wrong repeatedly because
`pgrep -f "…rustoid-compare --wiki"` matches the **wrapping shell** first, whose
command line contains the same string. Every `ps`/`sample`/`lldb` probe was
pointing at that shell, which is genuinely idle, so the run looked like a deadlock
at 0% CPU with an RSS of 1.7 MB. The real process was a sibling PID. Two
"deadlocks" were diagnosed from that artefact and "fixed" before the third
reading — this time over every matching process — showed 99% CPU.

The lesson worth keeping: a `-f` pattern match is not a pid lookup. The way to be
sure is to list *all* matches and check which one is doing work, or run the process
under a supervisor that reports it. Everything concluded from a single `head -1`
match was noise.

**What it turned out to be, and how to reproduce it in seconds.** The whole page
is a bad instrument: a run costs minutes, which makes a hypothesis expensive to
test. `rustoid-compare/examples/render.rs` renders a snippet from a file through
the same `WikiSiteConfig` and cache, so the taxobox alone can be run instead of
the article:

```bash
RUSTOID_CACHE_DIR=~/.cache/rustoid-compare \
  ./target/release/examples/render Zebra /tmp/taxobox.wt
```

That reproduces the blow-up, and a stack sample of the hung process names the
loop immediately: `expand_templates` → `expand_target_templates` →
`expand_templates` → …, with the *same* target string, `Taxonomy/`, at every
level and two children per node. Reading the offending token's own source settles
it:

```text
{{Taxonomy/\n<p><a rel="mw:WikiLink" href=":Template:Taxonomy/ Equus (Hippotigris)"></a></p>|machine code=parent}}
```

The template *title* is not a title. `Module:Autotaxobox` writes
`frame:expandTemplate{ title = 'Taxonomy/' .. taxon }`, and for a taxon whose
`Taxonomy/…` page is missing it reads the previous answer back — which is
**HTML** — and concatenates that into the next title. Rustoid then tokenizes the
HTML as wikitext, escapes it, expands again, and re-embeds it one level deeper;
the escaping compounds (`&amp;#39;`, `\\\"`), so the string roughly doubles
per round.

Three consequences, in order of usefulness:

1. **The limits bound it but do not make it practical.** The node counter caps
total work, and the depth bound caps nesting, but each node's *string* is
exponentially larger than the one before it, so the cap is reached only after
~1M expansions of a growing megabyte — minutes and gigabytes in a release build.
MediaWiki has the same two limits and the same hazard; it does not meet it on
`Zebra` because the template exists there. Which is to say the limits are
faithful but this is not the bug they fix.
2. **The strip above removed part of the fuel, not the mechanism.** The answer no
longer carries Parsoid's `{"src":"<p>"…}` JSON into the next title, which is
what made the escaping compound so fast, but a missing template still answers
with markup rather than a title, so the loop survives in slower form.
3. **The cache is still incomplete for this page.** `Template:Taxonomy/Equus
(Hippotigris)` was absent; one online run fetched it. Its parents in the chain
(`Taxonomy/Equus`, …, up to `Life`) are absent too. Zebra's own served HTML
contains **no** redlink to any `Template:` page, so on the wiki the whole chain
resolves — every offline failure here is a page that exists and was never
cached.

The next step is therefore two separate things, and they should not be confused:
fix the missing-template answer so it cannot be re-used as a title (MediaWiki
answers with the link text, and `template_to_wikilink` currently emits an
anchor with *no* content and a `:`-prefixed `href`), and populate the `Taxonomy/*`
chain so the offline run stops exercising a path the wiki never takes.

#### The first real scoreboard, and what it says to build next

With 44 of the 48 corpus pages cached (the other four, including the stalling
`Israel`, are recorded as skipped), the offline corpus run reports:

```
score: 0/44 compared (0.0%)
output: parsoid 57196568 bytes, rustoid 311357289 bytes (5.44x)

unexpanded wikitext (rustoid side):
  pages with literal {{...}}         43/44
  pages with literal {{#invoke:      43/44
  more literal syntax than parsoid   41/44
```

The second histogram is the one that matters. Every page but `Module:Math` — a
module *source* page, which by nature contains no `#invoke` — leaves Scribunto
calls in its output as literal text:

| page | rustoid `{{#invoke:` | rustoid `{{…}}` | parsoid `{{…}}` |
|---|---|---|---|
| COVID-19 pandemic | 16366 | 20942 | 34 |
| Cristiano Ronaldo | 3337 | 16381 | 51 |
| Hydrogen | 617 | 83626 | 1 |
| Quicksilver (film) | 18 | 289 | 2 |

Two conclusions, both measured rather than assumed:

1. **`#invoke` is the blocker, not a feature among many.** No page can match
   while `{{#invoke:…}}` is emitted as text, because those calls nest: one
   unexpanded call leaves the whole subtree of wikitext behind it, which is why
   the counts are enormous (Hydrogen: 83626) and why the output is 5.44x Parsoid's.
2. **The tag table is not yet a prioritisation tool.** Every tag reads `0/N`
   because a page carries several tags and contributes a failure to each, so
   `cite 0/40` does not mean citation handling is broken — it means 40
   cite-tagged pages fail, for any reason. Attributing by cause needs either
   single-tag entries or a per-page cause, which is what the `unexpanded`
   histogram is a first step towards.

The `unexpanded` metric exists because the first-difference classifier could not
do this job: it reported 41 of 44 pages as `transclusion`, because that marker is
simply what appears in the first 400 bytes of every article. "The infobox
rendered differently" and "the infobox was never rendered" need telling apart,
and only the latter is fixed by working on `#invoke`.

### Phase 3 — `#invoke` end to end (the biggest single lever) — *partly done*

- Wire `invoke` into the parser-function dispatch and build the module loader
  (`Module:` namespace, `require`, `package.loaders`, `mw.loadData`). — **done**:
  `#invoke` is intercepted in the async parser (it needs to fetch, and the
  parser-function path is synchronous), `require`/`mw.loadData` resolve from a
  preloaded registry, and `mw.getCurrentFrame` returns the live frame.
- Frame API, in this order: `frame.args`, `getParent`, `getTitle`,
  **`expandTemplate`**, **`callParserFunction`**, `preprocess`, `newChild`,
  `argumentPairs`. — `args` (both spellings), `argumentPairs`, `getTitle`,
  `getParent`, `extensionTag`, **`expandTemplate`**, **`callParserFunction`**
  and **`preprocess`** are in; `newChild` is not, and reports an error rather
  than returning something plausible-but-wrong.

#### How modules are fetched, and the trade that was made

Scribunto's `require` is a synchronous Lua call, so the modules a `#invoke` can
reach are **preloaded** before the module runs: the parser fetches the entry
module, scans its source for `require`/`mw.loadData` *string literals*, fetches
those, and repeats (bounded, and cycle-safe). `require` inside Lua is then a
registry lookup.

The trade, stated plainly: a module that computes a module name at runtime
(`require('Module:' .. name)`) cannot be anticipated, and its `require` fails
with a named error. The alternative — rewriting the Lua integration for async —
is a much larger change, and the literal form is what real modules use. The
scan also means `mw.title(...).exists` currently reports `true` rather than
consulting a data source; `LuaSite` is a deliberate snapshot of what Lua may
observe, and widening it is follow-up work.

- **Strip markers.** Scribunto protects non-wikitext output with `\127'"`UNIQ…`
  markers that must survive the round trip back through the parser. Getting this
  wrong produces subtly mangled output everywhere, so it belongs in this phase.
  — **not started**.
- **Exit criterion:** a page whose infobox is module-driven matches byte-exactly.
  — not yet; see below for where it stands.

#### Where `#invoke` stands after the second pass

The failure list keeps flattening, which is the useful signal:

| | occurrences | distinct | largest single entry |
|---|---|---|---|
| first `#invoke` wiring | 114 | 29 | 36 pages |
| after parent frames and `mw.clone` | 106 | 44 | 9 pages |
| after title facts and namespace fixes | 95 | 35 | 31 pages |
| after `mw.title` facts | 93 | 43 | 10 pages |
| after namespace aliases and `mw.html` chaining | 91 | 39 | 11 pages |

The 31-page wall turned out to be `ipairs(ns.aliases)` in
`Module:Namespace detect/data`: the field did not exist. A missing field *inside a
for iterator* is the nasty case, because Lua cannot attach a line to it, so it
appeared as a bare `attempt to index a nil value` against the whole module.
Reaching it needed a `pcall` around the module's *load*, which revealed the error
was raised at load time in the module's return table — every attempt to reach it
through an entry point had missed it entirely.

The other large win was `mw.html:done()` returning the **parent** node rather than
a string, as Scribunto's manual specifies. Modules chain
`:tag('li'):attr(..):done():wikitext(..)`, and returning a string broke the chain,
which made modules re-render whole subtrees. One line, and the corpus output ratio
fell from **5.55x to 3.50x** (219MB to 138MB).

Two diagnostics were added while chasing this, both of which paid off:

- `script_error` keeps the **first module frame** from Lua's traceback rather than
  dropping the whole trace. Without it, `attempt to index a nil value` carried no
  location at all and could not be attributed to anything.
- `mw.loadData` and `require` report the **type** of a non-string name rather than
  mlua's bare "error converting Lua table to String". A dynamic
  `mw.loadData(cfgModule)` previously produced a message naming no function.

`mw.title` now answers `exists`, `isRedirect`, `getContent` and `fullUrl`, using the
page's own source for `getCurrentTitle()` (free — rustoid is parsing it) and a
preloaded facts map for everything else. That map is built by scanning modules for
`mw.title.new('…')` literals, so a title computed at runtime reports as
non-existent; recorded as a known gap.

`mw.ext.data.get` returns an empty table rather than being nil: rustoid does not
implement the extension, and a nil `mw.ext` failed the whole page with an
unattributed error instead of just rendering nothing for that part.

#### `frame:expandTemplate`, `preprocess`, `callParserFunction` — done

These were the largest single item (**42 of the cached modules** reference at
least one, and `Module:Multiple image` alone accounted for 11 pages). They cannot
be done the way everything else was: `require` and `mw.title.new(...)` were
solvable by *preloading* because the titles involved are literals in the module
source, while `expandTemplate`'s **arguments are computed at runtime**, so the
result cannot be known before the module runs.

Lua calls them synchronously; `Parser::expand_templates` is `async`. Two options
were considered:

1. **Suspend and resume.** Each call records its request (title + args) and
yields; the host expands it outside Lua and re-runs the invocation with the
answers available. The same defer-and-retry shape the module preload already
uses, at the cost of running a module more than once.
2. **A synchronous inner parser.** Rejected: it duplicates the async pipeline,
and two implementations that can disagree make byte-exact parity harder rather
than easier.

**Option 1 is what was built** (`rustoid-core/src/pipeline/lua_deferred.rs`):

- The engine's frame gets `expandTemplate`/`preprocess`/`callParserFunction`
  as Rust closures that look the call up in a per-run answer map. A hit returns
the answer; a miss records the request and raises a marker error.
- `invoke` loops: run the module, and if it asked for something, expand that one
  request with the real async pipeline (a `template` token through
  `expand_templates`), record the answer, and re-run. Bounded, and a request that
  repeats without settling is reported rather than looped.
- Requests are keyed by **content** (title + sorted named args + positional
  args), not by call ordinal, so a re-run that reaches its calls in a different
  order still matches them up.

Five things had to be right, each found by a test:

- **The answer is HTML, not wikitext.** Scribunto's `mw.lua` binds these to
  `php.expandTemplate`/`php.preprocess`, whose PHP side runs the result through
  `recursiveTagParse` — so `'''bold'''` comes back as `<b>bold</b>`. An early
  version returned wikitext and broke every module that did
  `"prefix" .. frame:expandTemplate{…}`.
- **A parser function's first argument is joined by a colon**, not a pipe:
  `{{#uc:shout}}`, not `{{#uc|shout}}`. Getting this wrong made the call
  silently resolve to nothing.
- **The answer must not be the enclosing `#invoke` call.** Reusing the frame
  token's source made a *missing* template render back as
  `{{#invoke:T|main}}`; the module returned it, and the pipeline expanded it
  again, forever.
- **`mw:Transclusion` markers must be dropped** from the answer, both the
  opening marker and its `/End` partner — they are bookkeeping, and a module
  returning one puts it into the page text.
- **The engine's `pending` collector must be shared**, not recreated per run;
  a fresh `Rc` per `execute_in` left `take_pending` reading a different one, so
  no request was ever seen.

A `Cell<usize>` counter bounds nesting, because a module can reach the parser
only through these methods and the manual is explicit that its *output* is not
re-parsed for templates.

Wiring `#invoke` moved the corpus from "43 of 44 pages never expand their Lua
calls" to "Lua runs and the failures are a ranked list of what modules ask for".
Working that list down, in the order the scoreboard ranked it:

| failure | pages | fix |
|---|---|---|
| `require('strict')` / `libraryUtil` rejected | 36 + 26 | supplied as Lua source |
| `mw.ustring.match` and friends missing | many | forwards to Lua's `string` |
| `mw.site.namespaces` missing | 11 | built from the `LuaSite` snapshot |
| `mw.language.getContentLanguage` missing | 35 | added, colon-call tolerant |
| `bad argument #1 … expected string or number` | 37 | coercion, and the error now names the function |
| `isSubsting`, `listToText`, `subjectNamespaces` | 15 + 5 + 2 | added |
| `mw.html.create(nil)` rejected | 37 | `mw.html` rewritten as a real builder |
| `mw.html` `cssText` | 35 | added |

The shape of the remaining list says more than its length: the largest entry is
now 13 pages, and there are **45 distinct failures across 108 occurrences**,
where before there were two failures covering 62 pages. There is no longer a
single wall — what is left is breadth, which is Phase 4's job.

Two things to know about the numbers:

- The score is still 0/38 and the size ratio is 5.57x. Running Lua has not yet
  made a page *match*; it has replaced one kind of wrong output (literal
  wikitext) with another (partially rendered pages plus error text).
- `[string "module"]:13: attempt to index a nil value` and its siblings name no
  function. Those come from errors *inside* Lua code operating on a table the
  engine handed back with a missing field, so the message is a line number in a
  module. Attributing them needs either more `mw` surface or better traces.

### Phase 4 — Lua API breadth

- `mw.ustring` complete (`gsub`/`gmatch`/`find`/`match`/`sub`/`rep`/…), then
  `mw.title` completeness, `mw.site`, `mw.message` with real i18n lookup,
  `mw.text`, `mw.html`, `mw.language`, `mw.uri`.
- `mw.title:getContent`/`exists`/`redirectTarget` via `DataSource`;
  `mw.dumpObject`, `mw.log`, `mw.addWarning`, `mw.getCurrentFrame`.
- Instruction-count and memory limits enforced, with a wall-clock timeout.
- **Exit criterion:** the score on the module-heavy corpus moves materially.

#### Where the Lua errors stand

The offline corpus at this point holds 29 comparable pages and reports **19 Lua
failures across 18 distinct messages**, down from 87 when the phase began. The
remaining ones fall into three groups, which is the useful way to read them:

1. **`mw` surface gaps**, each small and well-specified: `mw.ext.ParserFunctions`
   and `mw.loadJsonData` (`Module:Math`, `Module:Music chart`),
   `mw.ustring.char`'s `U` sub-tag (`Module:Lang`, `Module:Wikt-lang`),
   `mw.unicode.scripts` returning a boolean instead of a table, and
   `Module:Piechart` / `Module:OSM Location map` indexing a nil.
2. **Engine compatibility:** `Module:Wikidata` fails to *parse* under Lua 5.4
   because of `"^\-"`, an escape sequence Lua 5.1 tolerated. That is a module
   written for the older dialect, not a rustoid bug, but it is a real
   compatibility question for the engine.
3. **`Module:Time ago`** subtracting one string from another, which Lua 5.1
   coerced. Same category as the above.

Two diagnostics earned their keep and are worth keeping in mind when reading
future failures: a nested load error must be attributed to the **innermost**
module (see `missing_module_from`), and a `for`-iterator cannot be a Rust
closure returning a tuple, because mlua drops the second value in that position
(see `mw.ustring.gcodepoint`).

#### Two wrong readings, and what they cost

Both groups above were, at some point, attributed to something else. Recording
them because each cost real time:

- The largest group was put down to **cache coverage**, on the theory that a
  computed `require` argument simply was not preloaded. The dependency was in
  fact *always* missable: `required_modules` only looked for a literal directly
  after the call, and `mw.loadData(sandbox('Module:Foo/config'))` wraps it.
  Teaching the scanner to read a title out of a computed argument removed every
  "was not preloaded" failure.
- "The page has no entity" was treated as a cache problem too. It was a *scope*
  bug: `expand_invoke` passed the invoking frame's title, so a `#invoke` inside a
  template asked `getEntityIdForCurrentPage` about the template. The fix is one
  field on `FrameContext`, and it is why exactly one sitelink lookup now runs per
  page instead of thirty.

### Phase 5 — On-wiki extensions

- **Cite first** (`<ref>`, `<references>`): names and groups, back-links, the
  `mw:Extension/ref` wrapper with its `data-mw` body, and the `<references>`
  list assembly. This is the most-used extension after Lua. — **done**
  (`rustoid-core/src/ext/cite.rs`; 23 tests). Runs as a DOM pass, which Cite's
  semantics force: a marker's number depends on refs appearing later in the
  document, and `<references>` renders the notes from wherever the tag sits.
- Then Poem, finishing Gallery, SyntaxHighlight/Source, Math, Templatedata;
  then the placeholder-semantics ones (Timeline, Graph, Mapframe).
  **Templatestyles is done** (see below).
- This needs the extension API widened from `-> Result<String>` to a token/DOM
  level so handlers can emit `mw:Extension/<name>` with `data-mw`/`data-parsoid`,
  and so they can be *hybrid* (part wikitext, part HTML) as Parsoid's are.
- **Exit criterion:** pages with references match.

#### Cite, and the node ids it depends on

Cite is the most-used extension after Lua — 28 of the 32 corpus pages carry a
`<ref>` — and it was the first thing to differ on the first page examined. On
`Zebra`, rustoid emitted **zero** `<sup>` markers against Parsoid's 62, with 46
raw `<ref>` elements left in the output. It now emits 45 markers and no leftovers.

Both halves are implemented (`rustoid-core/src/ext/cite.rs`; 23 tests). The shapes
were read off the cached Parsoid output, not guessed, which matters because Cite's
HTML is dense with derived ids:

- Numbers are assigned by **document order of first use**, so the markers and the
  note list cannot be computed independently.
- A named ref used again does not allocate a number; it appends a back-link to the
  existing note. The lookup key is the name when present and the content otherwise,
  which is also why two identical anonymous refs share a note.
- A note's body renders in the **list**, not at the call site. Rendering it in both
  places is the obvious wrong version.

Three output details a plausible guess gets wrong, all verified against `Zebra`:

- The marker and the note use **different separators**:
  `cite_ref-Badenhorst2019_1-0` but `cite_note-Badenhorst2019-1`.
- An anonymous ref still emits both separators around an empty name, giving
  `cite_ref-1-0` and `cite_note--1`.
- A note used **once** renders its back-link bare with `↑`; used twice or more, the
  links are wrapped in `mw-cite-backlink` and labelled `1`, `2`, …

**Cite is a DOM pass, and its semantics force that.** A marker's number depends on
refs appearing *after* it, and `<references>` renders the notes from wherever the
tag sits — usually the bottom of the page. So nothing can be decided while walking
past the refs, and the token pipeline, being a chain of independent passes, has
nowhere to keep the collection. One walk collects, a second substitutes. That is
why Cite could not be built the way the other extensions were — and why its
renderers build `Node`s rather than HTML strings: a string needed a raw-HTML escape
hatch to get past the serializer, and could not carry per-element ids at all.

#### The node ids, and how they turned out to be base64

Every element carrying `data-parsoid`/`data-mw` gets an `id="mwAQ"`-style
attribute, keying it in the page bundle a wiki serves. rustoid emitted **none**;
`Zebra`'s cached HTML has 3124.

The encoding is not what it looks like, and it cost two wrong attempts. It is
Parsoid's `Utils\CounterType::counterToId`: the counter written as **big-endian
bytes and then base64-encoded**, not as base-62 digits. Counter 1 is the byte
`0x01`, base64 `AQ`, giving `mwAQ`; counter 4 is `0x04` → `BA` → `mwBA`. The visible
`AQ, Ag, Aw, BA` cycle is base64's alphabet, not a conversion carry.

This is worth recording because base-62 is a *near miss* rather than an obvious
error: it reproduces the first 63 ids exactly and then diverges. I spent two rounds
reverse-engineering an alphabet that was never there before reading the source,
which would have settled it in one step. The lesson is the same one the `|`-in-attribute
false alarm taught: when a generated-artifact sequence looks arbitrary, find the
generator rather than infer it.

Three rules from the same source, each of which shifts every later id if wrong:

- Only an element with metadata to key takes a counter.
- An element that already has an id keeps it, and takes no counter.
- Pre-order depth-first from the body, starting at counter 1.

A fourth falls out of the allocation loop: a generated id colliding with one already
in the document is **skipped**, leaving a hole. `Zebra` has exactly two — 434→436 and
3026→3032 — so an implementation assuming a dense sequence matches 434 ids and then
drifts for the remaining 2600. All 3031 generated ids are reproduced exactly,
holes included, by replaying the collisions through the allocator.

**Ids are gated behind `ParserOptions::node_ids`, and that is a real distinction
rather than a test workaround.** MediaWiki's REST layer page-bundles Parsoid output
and assigns these ids; Parsoid in **standalone** mode emits none, and the fixture
suite compares against standalone output. Emitting them unconditionally dropped the
fixture score from 876 to **172** — the suite correctly reporting that standalone
output has no ids. `rustoid-compare` turns the option on, because matching what a
wiki serves is its entire purpose.

#### The magic words that take a colon but are not parser functions

`{{PROTECTIONLEVEL:edit}}` and `{{PROTECTIONEXPIRY:edit}}` look exactly like
parser functions and are resolved down a completely different path. They are
*core magic words*, so `siteinfo` lists them in `magicwords`, and
`resolve_target_string` checks that table **before** the parser-function one.
Two attempts to implement them as parser functions in
`call_parser_function` were therefore dead code that compiled, tested green,
and never ran.

The mock was what hid it. It registers no magic words by default, so the same
wikitext took the parser-function path in the test and the magic-word path on
the wiki. The tests passed and every live answer was empty — and an empty
answer is invisible, because that is *also* what an unprotected page returns.

The fix is small (the variable arm answers these two from the protection
context) and the lasting part is that the routing is now pinned by a test of
its own, and `MockSiteConfig::add_magic_word` is public so a test can register a
magic word the way a wiki does. **A mock that is easier to satisfy than the
wiki is a mock that certifies a bug**, and this one did so for several rounds.

A second, quieter failure in the same area: the wiki returns protection as a
flat `[{type, level, expiry}, …]` array, where the action is the `type` field.
Reading it as a map keyed by action deserialises to *nothing at all* rather than
erroring, so every page looked unprotected and the code path looked exercised.
The wire shape is now pinned by a test copied from a real response.

**A magic word has two spellings, and only one of them is visible in wikitext.**
`{{PROTECTIONEXPIRY:edit|Canada}}` and
`frame:callParserFunction('PROTECTIONEXPIRY', 'edit', 'Canada')` are the same
call, but the second is rendered back to wikitext by `render_call` as
`{{#PROTECTIONEXPIRY:edit|Canada}}` — **with a hash** — and re-expanded. So the
page-facing implementation is not enough: the hash spelling reaches the
parser-function arm, and an arm that does not know the name returns the call
*source* verbatim as "unknown parser function".

`Module:Effective protection expiry` then matched that source against
`'^(%d%d%d%d)(%d%d)(%d%d)(%d%d)(%d%d)(%d%d)$'`, failed, and raised
`internal error … malformed expiry timestamp` — on **34 of 48 corpus pages**.
That is the whole reason the failure count went *up* when
`title.protectionLevels` first landed: the module had been erroring earlier, at
the nil index, and fixing that exposed the next error behind it.

It also cannot be found by scanning the page. The call is built at runtime
inside the module, so the titles it names are not in the page's token stream at
all; they are collected in `expand_lua_request`, where the module's own call is
in hand and an await is available. Both modules in this pair are built entirely
from such calls, which makes them the first real test of that path.

#### `mw.wikibase` — implemented (the lookup surface)

The entities are read from Wikidata, which is a *different wiki* from the one
being parsed. `CachedDataSource` therefore holds an optional second wiki with
its own client and its own cache directory; the layout was already
`<root>/<host>/`, so nothing had to be restructured. An entity is one request
to `Special:EntityData/<id>.json`, whose body is the serialisation
`mw.wikibase` itself consumes; the page's entity is found with
`wbgetentities&sites=enwiki&titles=…`.

Lua cannot fetch, so entities are gathered before execution the way module
sources are. Two sources of ids are covered, and the second is the one that
matters:

- Literals in module source (`mw.wikibase.getLabel('Q42')`).
- **The page's own entity**, found by a sitelink *search*. A module calling
  `getEntityIdForCurrentPage()` never names an id, and `Module:Authority control`
  and `Module:Sister project links` both reach Wikidata this way.

The page title for that search is the **root** page, not the frame that made the
call: a `#invoke` inside a template asks about the article, while the invoking
frame is that template. `build_ast` records the real title and passes it through
`FrameContext::page_title`. Getting this wrong is not a small inefficiency — it
resolves the wrong entity — and it is worth knowing that the symptom was 30
sitelink lookups per page instead of one.

An unknown entity stays *absent* rather than becoming an empty table, so
`entityExists` answers false and the ~19 cached modules that guard on it take
their own fallback. That is what a wiki without Wikidata does, and it keeps the
no-entity path honest rather than rendering values that do not exist.

**Still missing, and the reason this is "the lookup surface":** `mw.wikibase.getEntity(id)`
returns an object, and the object methods beyond label/sitelink/statements
(`formatPropertyValues`, `formatStatements`) are not implemented.
`Module:WikidataIB` is 3538 lines and leans on those; it is the next step, not
this one.

#### The `subst` prefix, and what it was costing

Byte inflation was the largest visible symptom for a while: the corpus rendered
at ~7x Parsoid's size, one page at 10.1MB against 1.46MB. It was **not** a
serializer problem. `{{subst:Name}}` and `{{safesubst:Name}}` were not
recognised as preprocessor directives, so the whole `safesubst: Name` string was
taken for a page title, matched nothing, and the call's own argument list leaked
out as body text.

`Template:Country data X` is written this way — a `safesubst` call carrying a
hundred parameter lines — so every transclusion of it dumped those lines. On
Berlin that happened 117 times and produced 609 bogus "Template loop detected"
errors, which is why the leak looked like a loop-handling bug at first.

| | before | after |
|---|---|---|
| Berlin bytes | 10,115,822 | 2,681,375 |
| vs Parsoid (1,462,849) | 6.9x | 1.8x |
| "Template loop detected" | 609 | 0 |
| corpus output | 7.45x | ~2.0x |

The `<noinclude />` sits between the word and the colon, so it is still in the
token text when the target is parsed and recognising the directive removes it
too. Ground truth came from the live parser, which reports
`Template:Country_data_Germany` as the transclusion for
`{{safesubst: Country data Germany|flag}}`.

The remaining inflation is a *different* bug, and it is **fixed**: rustoid
stringified a parser function's branch value, so an HTML tag in it was escaped
and the element itself vanished.

```text
{{#ifeq:1|0|y|<div>hi</div>}}   rustoid: "hi"      parsoid: <p>y</p><div>hi</div>
```

Two causes, both fixed. `expand_kv` wrapped the branch in `Item::Str` after
`tokens_to_string` had flattened it; a token value is now returned as tokens. And
`prepare_pf_param_infos` built `data-mw` from stringified tokens where the
template path reads the argument's source range, so a tag disappeared from the
recorded wikitext as well.

`Template:Short description` is written as
`{{#ifeq:…|<div class="shortdescription">…</div>}}`, which is how this surfaced.
The two paths now agree with Parsoid byte-for-byte on a page mixing an inline and
a block branch.

#### Templatestyles — implemented

`Module:Citation/CS1` and a dozen others call
`frame:extensionTag('templatestyles', '', {src = …})`. That is lowered to
`{{#tag:templatestyles}}` and reaches the parser as an `<extension>`
placeholder; `Parser::expand_templatestyles` now expands it, rendering the
stylesheet with `pipeline::templatestyles`.

What is emitted, from the cached Parsoid output (the real contract, not a guess):

```html
<style data-mw-deduplicate="TemplateStyles:r1368532237"
       typeof="mw:Extension/templatestyles" about="#mwt3"
       data-mw='{"name":"templatestyles","attrs":{"src":"…"},"body":{"extsrc":""}}'>
  …the sanitised, minified CSS…
</style>
```

The revision id in the dedup key is the stylesheet's `revid`, so the inline text
and the key come from the same fetch (`DataSource::get_page_with_revision`).
The CSS **is** stored inline — an earlier note here claimed Parsoid emits nothing,
which is wrong: the fully scoped, minified stylesheet is the element's text.

A bare `src` such as `Hlist/styles.css` resolves to the Template namespace
(`$wgTemplateStylesDefaultNamespace`).

`pipeline::templatestyles::render` reproduces the sanitiser's transformations.
Every rule below was derived by diffing against Parsoid's own output, and
`rustoid-core/tests/templatestyles_parsoid_test.rs` re-checks all of them
byte-for-byte against the corpus cache:

- **Scoping.** `.mw-parser-output ` is prefixed onto each selector, except one
  beginning with `html` or `body` followed by a *descendant* combinator, where the
  scope is inserted after that prefix (documented in Extension:TemplateStyles —
  that is how a skin-dependent rule escapes the scope). A bare `body`, or
  `body > .x`, is scoped normally; qualifiers such as `:not(…)` belong to the
  prefix.
- **Declarations.** Whitespace around `:` goes, `! important` becomes
  `!important`, a trailing `;` before `}` is dropped. Spaces after `,` go
  (`rect(0, 0, 0, 0)` → `rect(0,0,0,0)`), and so does the space chaining one
  function to the next (`invert(1) brightness(55%)` →
  `invert(1)brightness(55%)`). Other internal spacing is meaningful and stays
  (`0 auto`, `12px/1.5`). Single quotes become double.
- **At-rule preludes.** A media *type* keeps the space after the at-rule name
  (`@media print{`, `@media screen and (…)`); a bare condition does not
  (`@media (max-width: 720px)` → `@media(max-width:720px)`). Within a condition,
  spaces after `:` and `,` go, and `and` loses the space before it when it
  follows a `)` — the one place that is special.
- **Comments** are removed entirely.

Two caveats on the Lua path, both recorded at `expand_invoke`:

- The expansion runs as an async pass in `build_ast`. A top-level `{{#invoke:}}`
  therefore works, but an `extensionTag` call made *inside* a template expansion
  cannot reach the fragment map without threading it through
  `expand_templates`' 13 recursive call sites. A `<style>` fragment that never
  reaches the tree builder is **silently dropped**, which is worse than leaving
  the tag unexpanded, so that threading was deliberately not done yet.
- The feature is inert offline: with no stylesheet pages cached, every corpus
  run leaves the placeholder alone and the score is unchanged. Populating the
  cache online is required to see any effect.

### Phase 6 — Integrated-mode semantics

- Close the gap to MW core's preprocessor where it differs from Parsoid
  standalone — the cause of the `+integrated`-only divergences already logged in
  `REMAINING-BUCKETS.md`.
- **Exit criterion:** the previously-"unreachable" fixture family becomes
  reachable and passing.

#### Integrated output strips `data-parsoid`, and that is why nothing matched

`rustoid-compare` reported **0 of 44 pages** matching, and the largest single
reason was not a parser gap at all. A wiki removes `data-parsoid` from Parsoid's
output before serving it; the cached `Zebra` HTML contains **zero** occurrences
of the attribute (while keeping 247 `data-mw` and 331 `rel="mw:WikiLink"`).
rustoid emitted it on every element, so the two renderings differed on the first
link of the first paragraph of *every* page. The scoreboard was measuring an
attribute, not a parser.

Parsoid's *standalone* output — what the fixture suite compares against — keeps
`data-parsoid` throughout (`quotes.txt` alone has three), so this is an
integrated/standalone difference in the same family as `node_ids`, and it is
gated the same way:

- `ParserOptions::strip_data_parsoid`, off by default, so the fixture suite and
every round-trip test keep their meaning;
- on in `rustoid-compare`, because its whole target is what a wiki serves.

The attribute is dropped **in the serializer** (`HtmlSerializer`), not by a walk
over the finished HTML. String surgery cannot tell an attribute from a page that
quotes one — an article about Parsoid, or a `<syntaxhighlight>` block, contains
the literal text — while the serializer knows the difference by construction.
`data-mw` is deliberately kept: it survives into served HTML.

`render_answer` — what a module receives from `frame:expandTemplate`,
`preprocess` and `callParserFunction` — strips it too, unconditionally. A wiki
would never hand that markup to a module either, and leaving it in is actively
harmful; see the blow-up section below, where it is the fuel.

#### The taxonomy walk, and three wrong answers on the way to it

`Module:Autotaxobox` walks a taxon's ancestry by calling
`frame:expandTemplate{ title = 'Taxonomy/' .. taxon }` and reading one field out
of the answer. Getting that chain right needed three separate corrections, and
two of the first three diagnoses were wrong — which is worth recording, because
each looked conclusive:

1. **A missing template must answer with its own title as text.** The module
   uses the answer as the *next* target, so an answer that reads as markup has
   the module splice that markup into a title, which the parser then
   re-tokenizes and re-embeds a level deeper. An anchor with no content — which
   is what rustoid produced — guaranteed exactly that. Fixed to Parsoid's
   fallback, `[[:$titleText]]` (`Parser::braceSubstitution`), so the link text
   is the full prefixed title. It goes through `add_red_links`, so
   `class="new"`, `data-mw-i18n` and `?action=edit&redlink=1` all come out
   right without a hand-built anchor.
2. **A cached body under a slash-escaped title was unreachable.** `get` looked
   up the key verbatim, but a body's filename escapes `/`, so
   `Template:Taxonomy/Equus_(Hippotigris)` was indexed as
   `Template:Taxonomy_Equus_(Hippotigris)` — and could never be found again.
   Every `Taxonomy/*` title is of that form, so the whole hierarchy read as
   absent while sitting on disk.

**Two diagnoses that were wrong, and the measurement that settled them.**
`Module:Autotaxobox` writes `{{Don't edit this line {{{machine code|}}} |rank=…}}`,
and `{{{machine code|}}}` is in the *target*, so rustoid was suspected of not
expanding an argument reference in a target's name. The live service disproved
it in one request: for a missing template it reports `href`
`./Template:SomePage` with `target.wt` `"SomePage {{{mc|}}}"`, i.e. the source
keeps the reference while the *href* is built from the expanded name. Two
further probes then showed the argument's value is genuinely part of the name —
`{{Some  Page {{{mc|}}}|r=1}}` resolves to `Some_Page` because title
normalisation collapses the runs, so there is no separator to trim — and that
`Template:Don't edit this line parent` **exists** on the wiki, as `{{{parent|}}}`.
So rustoid's name was right and the page was simply not cached. Three candidate
"parser bugs" were chased and all three were not bugs.

What made the difference was asking the live service for a specific href and
`target.wt` rather than reading the rendered HTML, which does not show the
distinction. `curl -X POST …/transform/wikitext/to/html/<page>` returns Parsoid's
own output for arbitrary wikitext, needs no login, and is the fastest ground
truth available for a question of this shape — one request replaced an hour of
token inspection.

**Still open.** A full taxobox run does not terminate. The loop is again
`expand_templates` → `expand_target_templates` → `expand_templates` with tokens
cloned at every level, and the sample is dominated by `realloc`/`memmove` — a
string growing without bound, the same escaping-compounding shape as before. The
taxonomy chain is now cached far enough to reach it, so the next step is to
trace *which* target recurses, exactly as the `Taxonomy/` case was traced.

**Still open, and where the taxobox walk now stands.** The loop is fixed for a
taxon whose chain is cached: `{{Automatic taxobox|taxon=Equus (Hippotigris)}}`
renders in 0.3s with no limit errors, walking 36 real taxa. What remains is the
case where a `Template:Taxonomy/*` page is **missing**.

**The two readings disagreed because they measured different things, and both
were right.** Resolved against the live service by reading the answer's bytes
through *two* nested `String.sub` calls, one of which supplies the outer bound
and so reveals the length by failing:

```
{{#invoke:String|sub|{{#invoke:String|sub|{{Taxonomy/NoSuchTaxonXYZ|machine code=parent}}|1|50}}|1|34}}
  -> [[Category:Errors reported by Modu
{{#invoke:String|sub|{{#invoke:String|sub|{{Taxonomy/NoSuchTaxonXYZ|machine code=parent}}|1|50}}|12|37}}
  -> Errors reported by Module
```

Both windows line up with one string and only one, and the outer `sub` reports
`String subset index out of range` at `1|40` — so the answer is **37 characters
of wikitext**:

```
[[Category:Errors reported by Module String]]
```

That is 11 (`[[Category:`) + 26 (`Errors reported by Module String`), and every
character is now accounted for. Three consequences, each of which changes the
port:

1. **The answer is wikitext, as the `render_answer` fix assumed** — not markup.
   The earlier reading that saw an anchor in the answer was reading the *page*:
   the enclosing `#invoke` parses the answer, and a `[[Category:…]]` line renders
   as a link-category element rather than as visible text, which is why the
   surface showed an anchor where the module held a link. A `#invoke` whose
   output is *only* that category line renders the anchor and nothing else.
2. **A missing template does not answer with the title text at all** when the
   call has arguments — it answers with the error-tracking category that
   `Module:Autotaxobox` *checks for by name*, and that check is what terminates
   the walk. `p.getTaxonInfoItem` does `pcall(frame.expandTemplate, …)`, finds
   `ok`, and then needs `info == ''`; supplying the category is how the module
   learns the template is absent. rustoid's `[[:$title]]` fallback satisfies
   neither the check nor the byte comparison.
3. **`MaxSearchLevels` is not the bound that fires here.** The module walks the
   ancestry until `getTaxonInfoItem` says the template is missing, so the wiki
   terminates on the *first* absent taxon. rustoid currently burns its whole
   1,000,000-node PP budget first (18.9s) and only then emits
   `Node-count limit exceeded`, which is a visible divergence rather than a
   slow path.

The shape is PHP's `Parser::braceSubstitution` error path: the category link is
prepended *before* the `$found = true` fallback, so both appear. The remaining
gaps in rustoid's missing-template branch are that the redlink marking is not
applied, `data-parsoid.pi` does not record the arguments, and `data-mw` loses
the target's `wt` and the parameter list.

A practical note for corpus runs: there is no per-page time bound, so one page
that does not terminate stalls the whole scoreboard. A wall-clock cap per page,
reported as `stalled` rather than as a failure, would have turned this
investigation's several hour-long waits into a single run. It is implemented now
(see the scoreboard below).

#### The first real scoreboard, and what it says to build next

The corpus had never produced a scoreboard, because one non-terminating page
stopped the whole run. With the stall cap in place it does, and the numbers are
worth recording because they are the first measurement of the port as a whole
rather than of one page.

| run | compared | stalled | literal `{{…}}` | Lua failures |
|---|---|---|---|---|
| before the name fix | 0/44 | (run never finished) | — | — |
| wikitext answers landed | 0/30 | 18 | 29/48 | 51 across 23 |
| `protectionLevels` landed | 0/48 | 0 | 47/48 | 91 across 33 |
| `TitleBlacklist` + magic words | 0/48 | 0 | 47/48 | 62 across 34 |
| `formatDate('U')` + `#` spelling | 0/48 | 0 | 47/48 | 49 across 33 |

Three things are worth reading off it.

**The score is 0, and that number is not yet informative.** Every page still
emits literal `{{…}}` (47 of 48), so every page diverges at the first
transclusion. The headline figure will stay 0 until that reaches zero on at
least one page, which makes the *secondary* numbers — stalls, Lua failures,
unexpanded counts — the ones that actually measure progress right now. The
scoreboard already reports them separately for exactly this reason.

**Stalls went to zero, and that is the real cost saving.** 18 pages previously
burned the full 1,000,000-node budget and 60s each. They now render. This is
what makes the corpus usable as a loop at all: a 48-page run takes minutes
rather than hours, so a change can be measured rather than guessed at.

**The Lua failure list has become a flat tail rather than a short head.** The
first count was 51 failures across 23 distinct messages with one at 17 pages;
after the fixes it is 49 across 33, with the largest at **6 pages** and most at
1—2. The total barely moved while the *shape* changed completely, and the shape
is the informative part: nothing systemic is left in the Lua layer, and what
remains is one module at a time. That is also why the count went *up* twice on
the way — fixing a blocked path exposes the next error behind it, so the number
of failures is not monotonic and should not be read as one.

**The exception to that is an error of my own, which is worth recording.**
`title.protectionLevels` first made the failure count go from 62 to 91, because
`Module:Effective protection expiry` calls the word through
`frame:callParserFunction` — which rustoid renders with a **leading hash** — and
the hash spelling was answered by neither arm, so it returned its own source and
the module could not parse a timestamp out of it. 34 pages. The lesson is that
a magic word has *two* spellings to a module, and only the page-facing one is
obvious.

**Where the remaining wrongness is, in one line.** The `transclusion` bucket is
45 of 48, and it is not a missing feature: it is *unexpanded wikitext surviving
into the output*. The pages render, at 2.37x the expected size — larger because
the wiki's output has been expanded and rustoid's has not. Fixing the first
template expansion is therefore the single highest-value next step, and the
per-page `data-mw` and `data-parsoid` differences are downstream of it rather
than independent problems.

#### Chasing the literal `{{…}}`, and what it actually was

That "fix the first template expansion" instruction was worth following literally,
because it turned out to be three separate things wearing one symptom. The
reduction that made it tractable was `{{Short description|Country in North
America}}` — the leading template of almost every article — which produced its
own body as visible `{{#ifeq:…}}` text.

**A raw `<` in a `data-mw` value ends the attribute.** `data-mw` is JSON inside
an HTML attribute, and a lenient parser stops the attribute at the first `<`
outside a quote. `Template:Short description` passes its own body — which
contains `<div class="shortdescription" …>` — as an `#ifeq` argument, so the
attribute closed early and the rest of the JSON spilled out as page text. This
was 455 literal `{{` on `Canada`, and it is why the page was 2.4x the expected
size: the template *was* expanding, and then its metadata was being printed.

The escaping differs between the two attributes, which is the part that is easy
to get wrong and that the fixture suite caught when it was first done
carelessly: `data-mw` writes `&apos;` where `data-parsoid` writes `&#39;`, both
escape `&`, and only `data-mw` needs `<`. `>` is left alone. It must also be
applied exactly once — in the JSON builder *and* the serializer double-encodes
every `&` to `&amp;lt;` — and the nested HTML inside a `data-mw`
`attribs[].html` field already carries its own escaping from when it was built.
Three fixture files regressed by 7 cases on the first attempt, which is what
pinned the rule down.

**A parser function's colon argument was recorded twice.** For
`{{#if:x|yes}}` the tokenizer puts the whole `#if:x` in `args[0].key`, so
`args[1..]` are the branches — but the parameter builder also split `x` out of
`target.wt` and prepended it, shifting every parameter by one. The live wiki
records that call as `params:{"1":{"wt":"yes"}}`. Two unit tests had encoded
the shifted numbering; both are corrected against the live service rather than
the other way round.

**`{{safesubst:ns:0}}` returned its own source.** `ns` is a registered function
hook, so inside a template it resolves as a *parser function* — and there was no
arm for it, so it fell through to the unknown-function fallback and echoed
itself. `Template:Main other` compares exactly that call against the empty
string to detect the main namespace, so it compared `"{{safesubst:ns:0}}"`
instead and took the wrong branch on every transclusion. Both spellings now
answer from `variable_value`, which already had the logic.

**What is left, and it is one thing.** `Canada` still shows 6,500 literal
`{{cite web}}`/`{{Cite book}}` — those are inside `<ref>` tags, so they are
Cite, a separate workstream, not a transclusion bug.

The other remaining item is encapsulation, and it is **not** the merge, which is
where the previous note said to look. Reducing it to three lines of wikitext
moved the diagnosis:

```
AAA{{If empty|<div>X</div>|b}}

wiki:     <p>AAA</p><div about="#mwt1" typeof="mw:Transclusion"
                            data-mw="If empty">X</div>
rustoid:  <p about="#mwt1" … data-mw="If empty">AAA
             <span about="#mwt2" … data-mw="#invoke:If empty">X</span></p>
```

Both give the outer template the right `data-mw`; what differs is the *range*.
The wiki's range is the `<div>` alone. rustoid's runs from the `<p>` — so it
absorbs the `AAA`, which precedes the template entirely, and then has nothing
left to put the inner parser function's content in but a second span.

The cause is upstream of encapsulation, in paragraph wrapping.
`ParagraphWrapper::open_p_tag` scans the pending output for a non-SOL-transparent
token to choose where `<p>` opens, then **overrides that choice backwards** to
sit before a `mw:Transclusion` start meta (`tpl_start_index`). That is what puts
the marker *inside* the `<p>`. Encapsulation then finds the marker by a nested
search from `children[0]`, takes the `<p>` as the range start, and stamps `about`
on every sibling through the end marker — including the text that came before
the template.

Two candidate fixes, and the second is the one to reach for:

1. `open_p_tag` should not pull the marker inside the `<p>`. Its comment says
   `tpl_start_index` is a faithful port, and the fixture suite (876/896) encodes
   that behaviour, so this needs the PHP source in hand rather than an inference
   about which direction the override should go. Two guesses here have already
   been wrong, and each was wrong in a way that looked locally reasonable.
2. `wrap_flipped_children` could refuse to extend the range *backwards* past the
   marker's own `dsr.start`. The marker's `dsr` is `[3,30]` on `AAA{{…}}` —
   exactly the template — so the text before offset 3 is provably not in the
   range, and the sibling holding it can be excluded on that evidence alone
   rather than on a heuristic. This is checkable per-range and does not depend on
   how the paragraph wrapper decided to nest things.

Recorded rather than attempted because it touches the range-finding rules that
most of the fixture suite depends on, and a wrong change there fails broadly
rather than locally. Forcing the body's `in_template` flag was tried and reverted:
it does not fix this and breaks `{{ {{T}} }}`, which legitimately keeps an inner
span.

#### Candidate 2 was tried, and the reduction shows why it cannot work

The guard was implemented — a range that would extend *backwards* is refused when
the sibling it would swallow ends at or before the start marker's `dsr.start`.
That is sound reasoning and it changes nothing here, because the `<p>` is not
before the marker: it is the marker's *parent*, and its `dsr` really does span the
whole thing.

A trace of the node `wrap_flipped_children` sees on `AAA{{If empty|<div>X</div>|b}}`
settles it:

```
child[0]  <p>    dsr [0,35]   about=None   typeof=None
child[1]  <meta> dsr [35,35]  about=#mwt1  typeof=mw:Transclusion/End
```

the start marker being nested inside `child[0]`, with its own `dsr` `[3,30]`.
So `dsr` agrees with the nesting, and no amount of checking `dsr` can separate
them.

#### The real cause: the `<div>` is not in the token stream at all

The paragraph wrapper is fed these tokens, in this order:

```
Str("AAA")
meta  mw:Transclusion
meta  mw:Transclusion
Str("X")
meta  mw:Transclusion/End
meta  mw:Transclusion/End
```

There is **no `<div>` token** — it exists only inside the transclusion's argument,
and materialises later, when encapsulation builds the DOM. So
`currLineBlockTagSeen` is never set, no block element closes the `<p>`, and the
`tplStartIndex` override in `openPTag` then pulls it back before the first
`mw:Transclusion` meta. The `<p>` swallows `AAA` and the whole range with it.

Live's `<p>` has `dsr [0,3]` and the `<div>` sits beside it, which means PHP's
`ParagraphWrapper` *did* see a block element. Checking the stage order explains
why — and this is the part worth remembering:

- `ParagraphWrapper` is a **token-level** handler, in
  `ParserPipelineFactory::STAGES['TokenTransform3']`, running *before* the tree
  builder.
- `PWrap` is a separate, **DOM-level** processor in the later
  `FullParseDOMTransform` stage.
- `TemplateHandler` (TT2) expands the argument, and `TokenStreamPatcher` (TT3,
  ahead of `ParagraphWrapper`) has already turned the argument's `<div>` into a
  real token by the time p-wrapping runs.

rustoid has the token-level port only, and reaches the same place by
encapsulating into the DOM *after* the tree exists. So the `<div>` reaches
rustoid's paragraph wrapper a stage too late.

**This is not a range-finding bug and cannot be fixed in the range-finder.**
Making the two agree means either splicing a transclusion's argument tokens into
the stream before the tree is built, or running encapsulation earlier — both
pipeline reorderings, not edits to `wrap_flipped_children`. The finding is
recorded here so the next attempt starts from the token stream rather than from
the range rules; three separate readings of this reduction were spent on the
latter.

#### The reduction, after the argument fix

Splicing the *argument* was the right instinct, but the missing `<div>` had a
different cause, and fixing that moved this a long way. Two commits:

**1. A module's arguments lost their tags.** `frame_args_to_lua` and
`kv_to_source_text` both called `tokensToString` on the argument, which has no
arm for a plain tag, so `{{#invoke:String|len|<div>X</div>}}` answered **1** where
the service answers **12**. The argument is now read from its `srcOffsets` — the
same technique template parameters already use. `<b>bold</b>` → 11 and
`[[link]]` → 8 came with it.

That is what put the `<div>` into the token stream, and with it the paragraph
wrapper now sees a block element: the `<p>` closes after `AAA` and the `<div>`
becomes a sibling carrying the transclusion, which is the structure the service
serves. **The recorded "stage ordering" conclusion was wrong on the cause** — the
`<div>` was not late, it had been dropped one step earlier.

**2. `about` was stamped on a sibling that precedes the transclusion.** With the
`<div>` in range, the `<p>` — which holds the start marker yet begins at offset 0
while the template starts at 3 — was stamped too, so the `<p>` and the `<div>`
shared one transclusion id. The service serves `<p id="mwAg">AAA</p>` with no
`about` at all. The stamp is now withheld when the sibling's `dsr.start` precedes
the marker's.

Narrowing the *range* to drop that sibling was tried first and cost five
fixtures: the range must stay contiguous for the span-wrapping and deletability
steps that follow, so only the stamp is conditional.

#### What is still wrong on the reduction

A node trace at encapsulation shows the finder **pairs the right markers and
still shapes the range wrongly**:

```
[0] <p>   dsr [0,0]    about=None   kids: Text("AAA"), meta #mwt1 (mw:Transclusion)
[1] <div> dsr [0,35]   about=#mwt2
[2] meta  dsr [35,35]  about=#mwt1  (mw:Transclusion/End)
```

The outer start marker `#mwt1` is nested in the `<p>` at `.1` and its end marker
is `[2]`, so the pair is correct. But the `<p>` is then taken as the range's start
*element*, so `lo = 0` and the range spans `<p>`, `<div>`, meta — where live's
spans only the `<div>` and the end marker.

Two ways to narrow that were tried, and **each cost five fixtures**:

- shrinking the range so the `<p>` is not in it, and
- moving the encapsulation *target* off the `<p>` onto the `<div>`.

Both disturb the span-wrapping and `is_deletable_in_range` steps that follow,
which assume the range is contiguous and starts where the finder said. Only
withholding the `about` stamp can be done without that collateral, which is what
landed. The next attempt should look at those two later steps rather than at the
range bounds — narrowing the bounds is the obvious move and it is the one that
does not work.

**This is the blocker for more than it looks.** Chasing the Cite gap on `Zebra`
(13 of its 158 refs missing) led to the same construct: the refs come from
`{{sfn}}`, which is `Module:Footnotes` through `Template:Sfn`, and on the page
they appear as literal `{{sfn|Plumb|Shaw|2018|p=54}}` inside a
`<p about="#mwt15" typeof="mw:Transclusion">` whose `data-mw` names
`Short_description/lowercasecheck` with `params:{"1":{"wt":"{{{1|}}}"}}`.

So the `{{sfn}}` refs are not a module problem at all — `{{sfn}}` works in
isolation, producing the right `cite_ref-FOOTNOTEPlumbShaw201854_1-0`. They are
collateral from the same `<p>`-swallows-the-transclusion behaviour — one
template's body concatenated into another's metadata, taking the page text with
it. Cite itself is byte-identical to the served page for every ref that
survives; the numbers differ (37 against 30) purely because the missing refs
shift the count.

The practical consequence is a priority: the range fix is not one page's cosmetic
difference, it is what unblocks Cite, `Module:Footnotes`, and the 200 literal
`{{sfn}}` on a single article.

#### Two parser bugs the corpus found, and one that turned out not to be one

Comparing `Zebra` against the wiki's own Parsoid turned up two real parser bugs.
A third was suspected in the same area and **investigated to a negative result**;
that is recorded too, because the negative is worth more than the suspicion was.

**A transcluded redirect was not followed.** Four of `Zebra`'s leading templates
(`Short description`, `Other uses`, `Featured article`, `Pp-semi`) rendered as a
link to themselves. `Template:Pp-semi` is `#REDIRECT [[Template:Protected page]]`,
and the redirect line was being rendered as article text. A template redirect is
ordinary wikitext (`{{db}}` → `Template:Delete`), so this was dropping a whole
class of the wiki's most-used templates, not one page. Three details mattered:
detection anchors at the *start* of the body (a body merely *mentioning*
`#REDIRECT` is not a redirect, and misreading one silently swaps in another
page's content); the target is a **full title**, so forcing the redirect's own
namespace onto it produced `Template:Template:Protected page`; and a redirect
whose target cannot be fetched is reported *unresolved* rather than falling back
to its own body, because rendering `#REDIRECT [[…]]` and its `[[Category:…]]`
trailer as text invents content Parsoid never emits.

**Arguments in HTML attributes took their default.** `Frame::expand` walked only
the top level of a token chunk, and an attribute value is a `KeyValue::Tokens`
*inside* the tag token — so `{{{small|no}}}` was never substituted during the
body expansion and was later resolved by `expand_attributes` against the **root**
frame, where the template's arguments do not exist. Both attribute fields had to
be walked, and the *key* field is the one that is easy to miss: a parser function
keeps its whole argument list in a single attribute whose key holds the tokens, so
handling only values left `{{#expr:{{{1}}}*2}}` evaluating the default.

**Not a bug: a `|` inside an attribute does not split a parser-function
argument.** This was logged as an open bug and it was wrong. `Template:Geological
range marker`'s `#ifexpr` branch contains `<div style="…">`, and the template
rendered nothing — so the `|` characters in that style looked like argument
separators. Tokenizing the attribute value directly shows the truth: the
`{{#if…}}` arrives as **one** `template` token with both its arguments intact, and
`split_template_args` never runs on an attribute value at all. The atom rule in
the tokenizer (a recognized HTML tag is consumed whole) already covers it.

The rendering was explained by two things that are both correct behaviour:

- `{{#if:1|B}}` yields `B`. That *is* `#if` — a non-empty condition takes the
  second argument. Reading `w:Bpx` as "the parser function was not evaluated"
  was a misreading of the output.
- `{{Real|5}}` binds `5` to `{{{1}}}`, so `{{{3}}}` is legitimately undefined and
  its default applies. Anonymous parameters are positional
  (`help:Templates`: identifying parameters by order "works only with anonymous
  parameters"), so a template calling `{{{3}}}` that way was never going to see
  the value.

The tests written while chasing it are kept as `rustoid-core/tests/
attribute_pipe_test.rs`, including the two behaviours that *look* like bugs and
are not, because the tokenizer's atom rule is load-bearing for real templates and
a regression in it would be silent.

The cost of this detour is worth recording: the misdiagnosis came from reading a
rendered attribute (`w:Bpx`) as evidence about the *tokenizer* rather than first
checking what the construct is supposed to produce. Tokenizing the input directly
settled it in one step, and should have been the first step.

### Phase 7 — Scale and hardening

- Large pages, deep transclusion, resource limits, parallelism, cache
  management, resumable batches, and a perf budget.

#### Expansion limits — *done*

MediaWiki bounds the preprocessor, and the bounds are three separate counters
with three separate trip conditions. All three are now implemented, and the
conditions are reproduced exactly rather than approximated, because the output
changes at the boundary:

| Limit | Default | Enforced in | Trip condition |
|---|---|---|---|
| `$wgMaxPPNodeCount` | 1000000 | `expand` | `++count > max` |
| `$wgMaxPPExpandDepth` | 100 | `expand` | `depth > max`, checked *before* the increment |
| `$wgMaxTemplateDepth` | 100 | `braceSubstitution` | `frame.depth >= max` |

The three details that decide whether a port is faithful:

- **The node counter counts `expand()` invocations, not AST nodes.** PHP returns
  early for a string argument, before incrementing, so plain text costs nothing,
  and its internal `$iteratorStack` walk never increments — a template whose body
  has 500 children costs **one** node, not 500. A repeat costs again: there is no
  memoisation on this path. rustoid charges one node per `template` token it is
  about to expand, which is the same accounting in a loop that walks a flat token
  list.
- **On a trip, an error span is substituted in place and parsing continues.**
  Neither limit aborts the page, and neither is sticky, so an over-limit page
  degrades into a cascade of spans rather than a failure. The strings are
  hardcoded PHP literals wrapped in `<span class="error">` — *not* the
  `<strong class="error">` the preview-only i18n messages use.
- **`$frame->depth` is not either of the other two.** The PHP source says so
  explicitly, and `loopAndDepthCheck` uses it for both the loop test (walking
  titles up the frame chain) and the recursion bound.

The limits are read from `SiteConfig::expansion_limits`, defaulting to MediaWiki's
own values. Nothing in `siteinfo` exposes them (they are not wiki settings a
client can read), so a wiki that overrides them needs a config override rather
than a fetch — worth knowing before looking for an API for it.

**One deliberate deviation.** PHP's `$expansionDepth` is a function-local
`static`: process-global, never reset by `clearState()`, and leaked outright if
the expansion unwinds abnormally. That is an implementation accident rather than
a semantic, and a parser reused across requests would carry the leftover depth
into the next one. rustoid keeps it per parse instead, reset at the start of
`build_ast`, which is indistinguishable within one parse. The counter is
recorded here because it is the one place the port knowingly differs.

## The `#invoke` `data-mw`, and why three separate readings of it were wrong

`Template:Infobox` is the smallest page in the corpus (341 bytes of wikitext) and
it exercised a parser-function path that the fixture suite cannot reach, because
standalone Parsoid implements no Scribunto. Reading the live transform endpoint
settled it in one request; four earlier readings from PHP source alone were each
wrong in a different way.

### What the service serves

| call | `parts` key | target | function | params |
|---|---|---|---|---|
| `{{#if:1|yes|no}}` | `template` | `{"wt":"#if:1"}` | `"function":"if"` | `{"1":"yes","2":"no"}` |
| `{{#invoke:String|len|abc}}` | `template` | `{"wt":"#invoke:String"}` | `"function":"invoke"` | `{"1":"len","2":"abc"}` |

Both are `typeof="mw:Transclusion"`, with no `mw:ParserFunction/*` type.

### The four wrong readings, recorded so they are not retaken

1. **`"parserfunction"` as the parts key.** PHP's `DataMw::toJsonArray`
   normalizes only `old-parserfunction` to `template`, so a `parserfunction`
   type keeps its own key — and an earlier test asserted exactly that. But the
   `parserfunction` type is only reached when a **modern PFragment handler**
   exists, or when `ParsoidExperimentalParserFunctionOutput` is set. Neither
   holds for `#invoke` on this wiki: Scribunto registers `invoke` through the
   legacy function-synonym table, so it arrives as `old-parserfunction` and the
   key is `template`. The old test encoded a configuration this wiki does not
   have.
2. **`function` vs `key`.** These are the same distinction from the other side:
   a genuine `old-parserfunction` writes `function`, a `parserfunction` writes
   `key`. Same source line, opposite conclusion, because reading (1) backwards.
3. **Folding the first argument onto the target.** `TemplateInfo::toJsonArray`
   really does `array_shift` the first param and append `':'.$firstArg->valueWt`
   to `target.wt` — *for an `old-parserfunction`*. Applying it unconditionally
   gave `wt = "#invoke:Infobox:infobox"` and an empty `params`, and then, when
   narrowed to `old-parserfunction`, silently dropped the first argument of
   every call instead. The live output keeps the colon in the target and every
   argument in `params`, so the fold does not apply at all here.
4. **`href`.** `put_opt_str` writes an explicit `"href":null` for a missing
   value, copying PHP's `?string` default. The service omits the key entirely.
   This is the kind of difference that survives a `data-mw` comparison done with
   a JSON parser and only shows up byte-for-byte.

The doubled `#invoke#invoke:String` had a mundane cause worth recording:
`pf_arg` is the text *after* `#invoke:`, while `target_str` is the whole
`#invoke:Module`; the target was built by prepending `#invoke` to a string that
already began with it. One is `"String|len|abc"` and the other `"#invoke:String"`,
and both are in scope at the same call site.

### `<includeonly>` consumes its content

The other half of that page's diff was not a range problem at all. The live
markup records the whole directive on the marker:

```html
<meta typeof="mw:Includes/IncludeOnly"
      data-mw='{"src":"&lt;includeonly>{{template other|...}}&lt;/includeonly>"}'/>
<meta typeof="mw:Includes/IncludeOnly/End"/>
<meta typeof="mw:Includes/NoInclude"/>
```

so the content is never parsed. rustoid emitted the markers and tokenized the
content anyway, and the leaked `{{template other|` text then set the paragraph
wrapper's `curr_line_has_wrappable_tokens`, opening a `<p>` around the whole
first line. **The `<p>` was a symptom of unexpanded wikitext, not of
p-wrapping** — the same causal chain recorded for `{{sfn}}` on `Zebra`, and the
reason `pages with literal {{...}}` in the scoreboard is the number to watch.

`<noinclude>` and `<onlyinclude>` do not consume their content; only the
`<includeonly>` case changes.

### Where `Template:Infobox` stands

The first difference is now at **byte 351**, and everything before it is
byte-identical to the served page — including the `<section>` wrapper and its
`id`, the `<span class="mw-empty-elt">` transclusion wrapper, and the opening of
the `<style>` an extension emitted:

```html
<section data-mw-section-id="0" id="mwAQ"><span class="mw-empty-elt"
  about="#mwt1" typeof="mw:Transclusion"
  data-mw='{"parts":[{"template":{"target":{"wt":"#invoke:Infobox",
            "function":"invoke"},"params":{"1":{"wt":"infobox"}},"i":0}}]}'
  id="mwAg"><style …
```

Three separate differences were fixed to get there, each recorded above or in the
commit history: the `data-mw` shape, the `<includeonly>` content, the attribute
order, and the section id. What remains is the `about` id on the `<style>`:
`#mwt2` live, `#mwt4` in rustoid.

### `about` ids are allocated out of document order

The `<style>` mismatch is not an off-by-two; it is a *sequencing* difference, and
the ids on the page show it. rustoid allocates, in order:

| id | element | why |
|---|---|---|
| `#mwt1` | the `#invoke` wrapper | the only one in the right place |
| `#mwt4` | the first `<style>` | but Live says `#mwt2` |
| `#mwt2` | the `Template:Documentation` wrapper | but Live says `#mwt3` |
| `#mwt5` | the second `<style>` | |

The cause is in `build_ast`: `expand_templatestyles` is a **token pass that runs
before the tree is built**, so it walks the whole stream and allocates an `about`
id for every stylesheet it can find — including stylesheets that are inside
templates which have not expanded yet, and so are not yet in document order.
Live allocates the id when the expansion *reaches* the stylesheet: inside the
`#invoke`, after the wrapper took `#mwt1`, which is what makes it `#mwt2`.

The trace narrows it to the expansion sequence rather than to the stylesheet pass
itself. With `RUSTOID_TRACE_TS=1` the allocations on that page are:

```
allocate #mwt1  in_tpl=false   the #invoke
allocate #mwt2  in_tpl=false   Template:Documentation
allocate #mwt3  in_tpl=false   #invoke:documentation
allocate #mwt4  in_tpl=true    #tag:templatestyles  → the style
allocate #mwt5  in_tpl=true    #tag:templatestyles  → the second style
```

Live serves only three ids (`#mwt1` the wrapper, `#mwt2` the style, `#mwt3`
`Template:Documentation`), so two of those five are ids PHP never hands out —
the `#tag` pair, which are `in_tpl=true` expansions that end up unencapsulated.
Note that PHP allocates in the `TemplateEncapsulator` **constructor**
(`$env->newAboutId()`), and `TemplateHandler::onTemplate` constructs one for
every template token, so "allocate then discard" is itself faithful; the
difference has to be in *which* calls reach that point, in what order.

#### The isolation that settles it

A bare `{{#invoke:Infobox|infobox}}`, with no `Template:Documentation` anywhere,
serves exactly two about ids:

```
about="#mwt1"    the transclusion wrapper
about="#mwt2"    the <style>
```

So the style's id is allocated **inside the `#invoke` expansion**, immediately
after the wrapper. The ordering is not a coincidence of how many other
templates exist on the page; it is where in the expansion the stylesheet is
reached.

#### Why the fix is not local

The id must therefore be taken while templates are expanding, and the blocker is
that the path which creates the token is synchronous: `frame:extensionTag`
lowers to `#tag`, `pf_tag` builds the extension token, and neither has the about
counter. `expand_templatestyles` — which *does* have the counter — is a separate
pass that runs after all expansion is done.

**Two parts of this were fixed.** The fragment now carries a deferred id that the
tree builder resolves where the wrapper tag actually sits, and the counter is
threaded through `expand_invoke`/`expand_lua_request` — it had been
`Cell::new(0)` per deferred call, so every module re-expansion restarted the
sequence at 1.

#### The count, not the order, is what is actually wrong

Measuring the *number* of about ids rather than reading the first few changes
the diagnosis materially:

| | about ids | of which `mw:Transclusion` |
|---|---|---|
| live | **140** | 2 |
| rustoid | **4** | 2 |

So the wrappers agree and the ids do not, because the 140 are almost entirely
extension elements — 93 `syntaxhighlight`, 43 `templatestyles`:

```
71  code  mw:Extension/syntaxhighlight
34  link  mw:Extension/templatestyles
22  div   mw:Extension/syntaxhighlight
 8  style mw:Extension/templatestyles
```

`Template:Documentation` is full of them, and rustoid is not producing those
elements at all — which is the same fact as the output being 4452 bytes against
live's 207945, not a separate one.

**The lesson is the one already recorded for `{{sfn}}`:** a handful of matching
ids at the start of a page says nothing, and counting the construct is what
distinguishes "numbered in the wrong order" from "not produced at all". Two
readings of this page were spent on the first explanation when the second was
true.

#### The extension elements, resolved

The 140 about ids trace back to `Module:Documentation` failing with

```
[string "Module:Documentation"]:555: attempt to call a nil value (method 'canonicalUrl')
```

Four separate module-observable facts had to exist for it to run, each one
invisible from the outside because the module reaches it through a `pcall` that
returns nil:

1. `mw.title:canonicalUrl{…}` did not exist. It is the `/w/index.php?title=…`
   form, server-relative, `%3A`/`+` encoded, `title` first; the shape was read
   off rendered pages rather than guessed.
2. `mw.site.namespaces[n].subject` was a **number**, not a table. Scribunto's is
   a namespace object and modules read `ns.subject.id`. A number there made
   `.subject.id` throw, which surfaced only as `subjectSpace = nil` →
   `templateTitle` nil → `docpageBase` nil → `docTitle` nil. Everything hung on
   that one field. `talk` has the same shape and was the same bug.
3. `preload_titles` parsed candidates with `Title::new_main`, so
   `Template:Infobox/doc` was fetched under a key with no namespace and never
   found again — `exists` read false for a page that was on disk.
4. `#invoke` argument values are expanded before the module sees them, while
   `data-mw` records them as written. Those are different strings and had been
   merged, so `data-mw` was recording the *answer* (`"4":{"wt":"2"}` where the
   service has `"4":{"wt":"{{#invoke:String|len|xy}}"}`).

`{{#invoke:documentation|main}}` went from 2203 bytes to 133390 and now renders
the transcluded `/doc` body, with `data-mw` byte-identical to the service's.

#### The last construct on this page

`Template:Documentation` calls the module with
`_content={{ {{#invoke:documentation|contentTitle}}}}`, and that argument is what
still does not resolve. Reducing it:

```
{{#invoke:String|len|{{ {{#invoke:documentation|contentTitle}}}}}}
  service: 36895   the /doc page, transcluded
  rustoid: 26      the literal text
```

`contentTitle` itself now resolves correctly. The failure is one level down and
is **not** about the computed target: `{{Template:Infobox/doc}}` in the same
position also fails, and a token dump shows why —

```
{{#invoke:String|len|{{Template:Foo}}}}    →  <template> key="#invoke:String" "" value="len" "" value=""
{{#invoke:String|len|a{{Template:Foo}}b}}  →  <template> … values "len" and "ab"
```

#### Retraction: the nested transclusion was never dropped

The paragraph above is **wrong**, and was written from a misread probe. That probe
iterated `t.attribs` and printed each `key`, so for `{{#invoke:String|len|a{{Yesno|yes}}b}}`
the *target* position (`Value::Str("#invoke:String")`) was mistaken for the argument.
A direct dump shows the tokenizer is correct:

```
{{Talk|a{{Template:Foo}}b}}  →  Value::Tokens(3) = Str("a"), template("{{Template:Foo}}"), Str("b")
```

and `data-mw` records `"1":{"wt":"a{{Yesno|yes}}b"}`. Nothing is lost at
tokenization. The real defect was in the *parser-function target*, above.

#### The real bug: `data-mw` recorded the expanded target

`{{#switch: {{lc:YES}} |yes=FOUND |#default=MISS}}` — the switch **did** answer
`FOUND`, so the argument was expanding fine. What differed was `data-mw`:

| | `target.wt` |
|---|---|
| service | `#switch: {{lc:YES}} ` |
| rustoid | `#switch: yes` |

The recorded target must be the source **as written**, not as expanded — the same
rule already applied to argument values, which read from `srcOffsets`. rustoid
built it from the expanded `target_str` that `resolve_template_target` consumes,
so any parser function whose colon argument held a template recorded the answer.

That is `Template:Yesno`, whose body is
`{{<includeonly>safesubst:</includeonly>#switch: {{<includeonly>safesubst:</includeonly>lc: {{{1|¬}}} }} |…}}`,
and through it `Template:Documentation`, `Template:Main other`, and every infobox.

The fix reads the raw target back out of `DataParsoid::src` (`raw_template_target`)
and records that, with `strip_include_directives` applied. The directive stripping
is what makes the two spellings agree: the service records
`{{#if:<includeonly>X</includeonly> |yes|no}}` as `"#if: "`, and
`#if:<!-- c -->X |yes|no` keeps the comment as literal text. Checked both.

One thing that *was* right in the old guess: `strip_subst_prefix` is genuinely
load-bearing here, and it is correct as it stands. `{{<includeonly>safesubst:</includeonly>lc: y}}`
really does resolve to the magic word `lc` — the service reports
`{"target":{"wt":"lc: y","function":"lc"},"params":{}}` — because `subst` is a
title prefix. It fires only when the whole target is a plain title, so a colon
further in (`Foo:subst:Bar`) or a nested `{{…}}` in the argument leaves it alone.
Narrowing it to titles-only was the right call and cost no fixtures.

Status: fixtures held at exactly 876/896. `Template:Infobox` moved from 6722
bytes toward the served 209692; the scoreboard delta is recorded below.

## The stall cap, and the four loops under it

A corpus run used to hang on page 1 and print nothing. The cap was the reason
it could not be diagnosed, so it was fixed first, then used.

### The cap never fired

`tokio::time::timeout` only cancels at an `.await` point. A non-terminating
expansion is synchronous CPU work that never yields, so `Cristiano Ronaldo` sat
at 100% CPU for nine minutes with the cap set to 60s and the corpus never
reached page 2 — a scoreboard that never prints measures nothing.

The render now runs on a blocking worker and its handle is raced against the
timer. `spawn_blocking` needs `'static`, so `WikiSiteConfig` gained `Clone` and
the render takes an owned input. A `std::thread::scope` borrows fine but its
*implicit join at the end of the scope* re-blocks on the very thread the timer
is abandoning, which defeats the cap — worth knowing, because it looks correct.

An abandoned worker cannot be stopped in safe Rust. That leaks a core per
stall, so the harness counts them and refuses to start more past two. The
refusal is a real result, not a workaround: six stalled workers at ~600% CPU
then starved `Template:Infobox` of a thread and it hung for 22 minutes *with a
60s cap*, because its render never started.

### `Module:Documentation` and the subpages no one preloaded

`contentTitle` returned the empty string. Patching the cached module to print
its state gave the answer in one run: `prefixed=Template:Yesno/doc` but
`exists=false`. The title was right and the *fact* was missing.

`preload_titles` preloaded `/doc`, `/sandbox` and `/testcases` for the **root
page only**. `Module:Documentation` is transcluded from another template's body
— `{{Yesno}}` ends with `{{Documentation}}` — so its `env.title` is
`Template:Yesno`, not the page being rendered. The page it needs had never been
asked for, `exists` was false, and the module took its "does not exist" branch.

The lesson generalises: `page_title` and `getCurrentTitle()` are different
titles, and a module that documents *its caller* needs the caller's subpages.

### `parent.args` were handed over as source, not expanded

Scribunto gives a module expanded text. rustoid passed `frame:getParent().args`
raw, so the literal string `{{If empty|…}}` sat inside `parent.args`;
`Module:Infobox` expands what it reads, and each expansion re-entered the parser
and re-invoked the module. Fifty lines of exponential-looking stall, one wrong
stringification.

The tell was that expansions stayed bounded at 44 while fetch counts ran into
the hundreds: **the expansion depth never grew, so the depth limit never
fired.** A flat loop with growing breadth is a different animal from recursion,
and the depth counter cannot see it.

### Title facts are per render, not per invoke

`preload_titles` runs once per `#invoke`, and its inputs barely change between
calls, so every call re-fetched the same subpages: `{{Infobox person}}` asked
for `Sandbox/doc` 29 times over 357 fetches. The facts cannot change mid-parse,
so a render-wide cache is a dedupe rather than a heuristic. 357 → 201.

### `{{Infobox person}}` did not terminate — solved

This was the longest-running failure in the port, and the earlier notes on it were
wrong. Both attempts at it were aimed at the wrong thing.

**What was misleading.** Expansions stayed bounded at 44 and `frame.depth()`
stayed at 2 throughout, so it looked like a breadth loop with no depth to catch —
and the per-invoke guards (`MAX_FRAME_ROUNDS`, the `asked` repeat check) never
fired, because each round requested a *different* key. The guess recorded below,
that `Module:Arguments`' `wrappers` option was rebuilding `getParent().args`, was
wrong: `frame_common` builds a plain table and never re-expands.

**What it actually was.** The profiler finally showed `find_template_closing` ↔
`skip_tplarg` 85 frames deep, and a depth-40 `exit(9)` that dumped its input gave
the answer: 1828 bytes of `<td>` cells holding `{{{…`, with 29 `{{` and **zero**
`}}`. A module's HTML table output looks exactly like that to this scan.

`Module:Infobox` emits rows whose cells hold a template call. The parser sees a
`{{` with no closer anywhere ahead, and `skip_tplarg` recurses into every nested
`{{{` it meets — each of which scans to the end of the input and then recurses
again. Exponential, with a bounded *depth* and a bounded *expansion count*. That
combination is what hid it from every counter the port already had, and it is why
two attempts at bounding the recursion changed nothing and dropped the fixtures
to 874.

**The fix.** The scans are deterministic, so "nothing closes from offset `p`" is
a fact that cannot change while the same string is scanned. One `ScanMemo`,
shared by both scanners for the whole scan, makes the walk linear.

The sharing is the load-bearing part. The first attempt at the memo keyed it
per `skip_tplarg` call, which discards it at the exact moment the other scanner
needs it — the two re-enter each other, so a memo that is not shared is no memo
at all.

**On the regression guard.** The tests pin the mechanism — a failed offset is
recorded, and a successful `{{{…}}}` is not poisoned by another input's failure —
not the input. The blow-up needs `Module:Infobox`'s expansion state, and the same
text renders promptly outside it; verified by disabling the memo, which stalls
`{{Infobox person}}` past 120s while every snippet tried in isolation is
unaffected. The end-to-end guard is therefore the corpus, and the unit tests
cover the invariant, which the corpus would be slow to attribute.

**The lesson worth keeping.** A count being bounded does not mean the work is.
Depth limits and node counts both looked healthy while the scan ran forever, and
"the counters are fine" was read as "this is not a tokenizer problem" for two
sessions. When something does not terminate but every instrumented counter does,
the instrument to reach for is the profiler, not a third counter — and a depth
guard that dumps its input is worth more than one that merely reports a count.

## The first real scoreboard

Measured on a fresh `rustoid-compare` binary — check the timestamp first, because
a stale one silently reports the previous build's results and that mistake was
made once here already.

```
score: 0/46 compared (0.0%), 2 stalled
output: parsoid 64435613 bytes, rustoid 163885024 bytes (2.54x)

unexpanded wikitext (rustoid side):
  pages with literal {{...}}         45/48
  pages with literal {{#invoke:      44/48
  more literal syntax than parsoid   43/48

lua failures (31 across 21 distinct):
     7 pages  Module:Unicode data:485: attempt to index a boolean value (field 'scripts')
     2 pages  Module:Check for unknown parameters:195: attempt to index a nil value
     2 pages  Module:Hatnote inline:16: attempt to call a nil value (method 'newChild')
     2 pages  Module:Piechart:220: invalid piechart data: parseMetaParams
     2 pages  Module:Time ago:62: attempt to sub a 'string' with a 'string'
     1 page   Module:Wikidata:247: invalid escape sequence near '"^\-'
     1 page   Module:Country alias:228: attempt to index a nil value
     1 page   Module:Location map:620: cannot find data/Pacific Ocean
     1 page   Module:Math:363: bad argument #1 to 'log10' (number expected, got string)
     1 page   Module:Math:389: attempt to perform 'n%0'
     1 page   Module:Multiple image:177: arithmetic on a nil value (local 'totalwidth')
```

Zero matches is not the interesting number. These are:

- **`45/48` pages still contain literal `{{...}}`.** A page that never expands
  cannot match, and this says the failure is wholesale rather than a
  serialization detail. The `lua failures` list is the cause: a module that
  raises is replaced by the script-error markup, and everything it would have
  emitted stays as source.
- **rustoid emits `2.54x` parsoid's bytes.** Un-expanded wikitext is left in
  place *and* the script errors are emitted, so the page grows instead of
  shrinking.

### What changed since the previous scoreboard

| | session start | after the tokenizer fix | now |
|---|---|---|---|
| compared | 0/40 | 0/46 | 0/46 |
| stalled | 7 | 2 | 2 |
| literal `{{...}}` | 39/47 | 45/48 | 45/48 |
| distinct Lua failures | 20 | 21 | **18** |
| entries in the Lua table | 34 | 31 | **21** |

Four pages that used to stall now render, and the Lua table has lost a third of
its entries. Every one of these was a named, checkable defect:

- `Module:Lang` (`ustring.char`, 7 pages) — fixed. It took bytes where Scribunto
  takes codepoints.
- `mw.html` (attribute table, 4 pages) — fixed, and the nil-value case with it.
- `Module:Time ago` (2 pages) — fixed, and it needed *two* fixes: the format
  string and then the input parsing. It left the table only on the second run,
  which is the sort of thing that reads as a failed fix.
- `Module:Unicode data` (7 pages, the top entry) — fixed. It builds data-module
  names at runtime and guards the load with `pcall`, so the static scan could not
  see the name and the `pcall` swallowed the error the retry loop keys on. The
  name is now collected as a side effect, where a `pcall` cannot eat it.

The top entry is now `Module:Check for unknown parameters` at 2 pages — the
table has no dominant cause left, which is the shape a work queue takes when it
is nearly drained.

### The Lua failures are the work queue

They are concrete, each names a line, and the biggest is worth 7 pages:

- `Module:Unicode data:485` — **fixed.** It was the top entry at 7 pages, and it is
the most interesting one in the table. The module builds its data-module names at
runtime:

  ```lua
  local loader = setmetatable({}, {
      __index = function (self, key)
          local success, data = pcall(mw.loadData, "Module:Unicode data/" .. key)
          if not success then data = false end
          self[key] = data
          return data
      end
  })
  ```

  Scribunto does not care: `mw.loadData` fetches synchronously, so the
  concatenated title resolves on demand. rustoid **preloads**, and the static scan
  can only see the prefix `"Module:Unicode data/"` — the rest is computed. So the
  data was absent, `data = false`, and the caller indexed it.

  Two mechanisms blocked the existing fix at once, which is why this one took a
  while: the scan cannot see the name, and the `pcall` swallows the
  `"module … was not preloaded"` error that the retry loop keys on. Reporting the
  name *out of band* — `require` records it in a registry slot before raising, and
  the run surfaces it as `Outcome::MissingModule` — gets it past both. The loop
  then fetches and re-runs, which is the path an uncaught `require` already used.

  `{{#invoke:Unicode data|lookup|script|41}}` answers `Latn`, matching the service.
  Two data submodules still answer differently (`block`, `name`); that is a
  data-shape question rather than a fetch one and is the next thing to look at
  there.
- `Module:Wikidata:247` — `invalid escape sequence near '"^\-'`. **This one was an
  interpreter-version gap, not a module bug.**

  The line is `mw.ustring.match(date, "^\-?%d+")`, and the service renders it
  with no script error. Real Lua *5.4* rejects that escape — the `lua` binary here
  is 5.5 and rejects it too — but Scribunto runs **Lua 5.1**, which treats an
  unknown escape as the bare character. rustoid embedded `lua54`:

  ```toml
  mlua = { version = "0.10", features = ["lua54", "vendored"] }
  ```

### Lua 5.1 — settled by the documentation, not by preference

This was left as "a deliberate call rather than a patch" pending a decision.
`Extension:Scribunto` answers it outright:

> Only binary files for Lua **5.1.x** are supported. LuaJIT, although
> theoretically compatible, is not supported.

The bundled binary is `lua5_1_5_linux_64_generic`, i.e. **Lua 5.1.5**. So 5.1 is
the *contract* Wikipedia's modules are written against, not a preference, and the
switch is a fidelity fix. The feature is now `lua51`.

Two things had to be checked rather than assumed:

- **The lexer.** Lua 5.1 has no `\xNN` escape; it has decimal `\ddd`. Probing the
  embedded engine confirms rustoid's lexer already reproduces 5.1 exactly:
  `'\195\169'` is 2 bytes, `'\xc3\xa9'` is the 6 literal characters `xc3xa9`,
  `'\255'` is 1 byte, and `'\256'` raises `escape sequence too large` — all of
  which are what `llex.c`'s `read_string` does.
- **The one failing test** was `ustring_sub_clamps_instead_of_panicking`, and its
  expectation, not the engine, was at fault: it wrote the é of `héllo` as
  `'h\xc3\xa9llo'`, which in 5.1 lexes as `hxc3xa9llo`, and then sliced that to
  `xc`. Rewritten with `'h\195\169llo'`; the comments attributing the values to
  "Lua 5.4's `string.sub`" now say 5.1, which is the dialect Scribunto actually
  runs. The `string.gfind` shim's comment (which claimed "Lua 5.4 dropped the
  alias") was corrected for the same reason.

### Title comparison was broken for every title — two separate causes

The switch exposed `titles_compare_by_identity`, which had been failing all
along. Reproducing it outside the parser gave the decisive clue: `getmetatable(t)`
*had* an `__lt`, yet `c < a` still raised `attempt to compare two table values`.

**Cause 1 — one metatable per title.** Lua 5.1 only dispatches a relational
metamethod when *both* operands carry it. `mw.title.new('Foo')` built a fresh
metatable per call, so `getmetatable(a) == getmetatable(b)` was false and no
title could ever be compared with another. Worse, the metatable was *also* where
the per-title `__index` closure lived (`facts`, `is_current`, `current_source`),
so the two requirements pulled in opposite directions. The fix separates them:
the metatable is built once and cached in the registry, and the per-title data is
stored on the instance as a closure under `TITLE_FACTS_KEY`, which the generic
`__index` invokes. Every construction path (`mw.title.new`, `subPageTitle`, the
`*PageTitle` accessors) now shares the one metatable.

**Cause 2 — `__eq` identity, which is a *mlua* bug.** With the metatable shared,
`<` worked and `==` still did not. The cause is in mlua 0.10.5's
`Table::equals`:

```rust
// Compare using `__eq` metamethod if exists
if let Some(mt) = self.metatable() { if mt.contains_key("__eq")? { return mt.get::<Function>("__eq")?.call((self, other)); } }
if let Some(mt) = other.metatable() { if mt.contains_key("__eq")? { return mt.get::<Function>("__eq")?.call((self, other)); } }
```

It fires the metamethod if *either* operand defines one. Lua 5.1 requires the
metamethod to be the *same function* on both (the reference manual, footnote ‡:
"the metamethod is only used if the same function is specified in both
arguments' metatables"). The difference is observable, and a control experiment
pins it down — two tables with distinct-but-identical `__eq` closures:

| case | rustoid before | correct 5.1 |
|---|---|---|
| `__eq`, same metatable | `true` | `true` |
| `__eq`, distinct metatables | `true` | **`false`** |

A shared metatable cannot fix this, because a class-based `__eq` is a *different
function* from the dispatcher that needs to see it. The resolution keeps the
dispatcher as the one shared `__eq`: each comparison class registers its
predicate and is tagged, instances are tagged to match, and the dispatcher
compares only same-class objects. Because the tag is on the instances and the
dispatcher is one function on the shared metatable, the identity rule falls out
for free — and it deliberately does *not* install `__eq` on the class metatable,
since doing so would make `title == {}` succeed where 5.1 raises.

That this is the *correct* fix is worth stating plainly: a title is not a Lua
table with a metamethod, it is an object whose `__index` happens to be a
metatable — so overloading it to compare a title against an unrelated table is not
something the 5.1 identity rule would ever have permitted.

### An environment failure that is not a regression

`templatestyles_parsoid_test` fails with "cache holds no stylesheet sources". It
defaults to `/tmp/rustoid-cache` and looks for `html__*`/`page__*` files, while
the compare cache uses `page:*` under `~/.cache/rustoid-compare`. It was not
touched by this work (last modified in `ef5213c`) and no Lua change can affect
it. Left alone rather than "repaired" blind; it needs the two cache layouts
reconciled, which is a separate job.
- `Module:Time ago:62` — **fixed, twice.** First the `formatDate('xnU')` format
  string (see the raw-prefix note above); then the two input defects: it did not
  *raise* on an unparseable stamp (so `Module:Time ago`'s `pcall` succeeded and
  the error became a subtraction), and it parsed only `YYYY-MM-DD`. A year-only
  input like `2020` is common on the corpus and the service accepts it.
  `{{#invoke:Time ago|main|nonsense}}` now renders the module's own
  `Error: first parameter cannot be parsed as a date or time.`, byte for byte
  with the service.

  Worth noting how that one was found: the scoreboard still listed it *after* the
  format-string fix, which looked like the fix had failed. It had not — the same
  line has two independent defects, and the second only shows for inputs the
  first one does not cover. A stale binary was the other candidate and was ruled
  out by checking the timestamp first.
- `Module:Math:389` — `attempt to perform 'n%0'`. Checked: Lua 5.4 raises this too,
  so rustoid is right and the input is what differs. Not an engine coercion.
- `Module:Multiple image:177` — arithmetic on a nil `totalwidth`. Lua coerces
  numeric strings in arithmetic (`'100' - '40'` is 60, verified); worth checking
  whether the nil comes from a coerced-string gap or from an earlier value
  difference.

### Scoreboard after the Lua 5.1 switch

Lua failure entries: **21 → 17**, and the distinct failure strings went from 20 to
17 while the *page* count fell from 100 to 48. The `Module:Wikidata` escape
failure is gone, as predicted. Progress, but the headline score is still `0/46`:
the big entries are all downstream of one or two root causes.

```
score: 0/46 compared (0.0%), 2 stalled
output: parsoid 64435613 bytes, rustoid 162291287 bytes (2.52x)
unexpanded wikitext: pages with literal {{...}} = 45/48
lua failures (48 across 17 distinct):
    23 pages  Module:Citation/CS1:832: malformed pattern (missing ']')
     7 pages  Module:Main list:28: bad argument #2 to 'format' (string expected, got nil)
     2 pages  Module:Check for unknown parameters:195: attempt to index a nil value
     2 pages  Module:Hatnote inline:16: attempt to call method 'newChild' (a nil value)
     2 pages  Module:Piechart:220: invalid piechart data: parseMetaParams
     1 page   Module:Country alias:228: attempt to index a nil value
     1 page   Module:Location map:620: cannot find data/Pacific Ocean
     1 page   Module:Math:100: bad argument #1 to 'random' (interval is empty)
     1 page   Module:Math:363: bad argument #1 to 'log10' (number expected, got string)
     1 page   Module:Multiple image:177: arithmetic on a nil value (local 'totalwidth')
     1 page   Module:Multiple image:340: attempt to index a nil value
     1 page   Module:Music chart:1736: attempt to call field 'loadJsonData' (a nil value)
```

The next targets, in order of pages affected:

1. **`Module:Citation/CS1:832` — `malformed pattern (missing ']')` (23 pages).**
   The single biggest entry, and a *pattern-dialect* question rather than a module
   bug: it is `mw.ustring` pattern syntax, so the mismatch is either in rustoid's
   pattern translation or in the string it is matching against. Worth checking
   the exact expression against the live service before theorising — the same
   discipline that settled the Lua version.
2. **`Module:Main list:28` — `format` got nil (7 pages).** A `string.format`
   argument that rustoid did not produce, so an upstream value differs.
3. **`Module:Music chart:1736` — `loadJsonData` is nil (1 page, but newly
   visible).** A genuine missing API: `mw.loadJsonData` (or the module's own
   wrapper) was never implemented. Cheap and self-contained.

The `by tag` table is still all zeros, which is expected while literal `{{...}}`
remains on 45/48 pages: a page cannot byte-match while still showing its source.
The unexpanded count is therefore the metric to watch until it starts falling.

## The `mw.ustring` split, and the NUL byte in `Module:Citation/CS1`

`Module:Citation/CS1:832` is the single largest entry in the failure table (23
pages), and the cause is not a missing edge case but a structural one.

**Isolation.** The line is `mw.ustring.find(v, pattern)`, where `pattern` comes
from `cfg.invisible_chars`. Testing all fourteen of those patterns separately
showed exactly one failing:

```lua
{'C0 control', '[\000-\008\011\012\014-\031]'},
```

Under Lua 5.1 a `\ddd` escape is *decimal*, so `\000` is a real NUL byte
(measured: `#'\000'` is `1`, `string.byte('\000')` is `0`). Lua 5.1's `classend`
scans a `[...]` set until `*p == '\0'`:

```c
if (*p == '\0')
  luaL_error(ms->L, "malformed pattern (missing " LUA_QL("]") ")");
```

so a NUL anywhere inside a set raises. Reduced to the minimum, `string.find('abc',
'[\000]')` fails, while a *bare* `\000` pattern succeeds — it goes through
`singlematch`, never `classend`.

**The measurement that changed the fix.** Live returns `0`, not an error, for
both `[\000]` and `[\000-\008]`. So Scribunto does *not* have this bug, and the
defect is rustoid's: `mw.ustring`'s metatable forwards everything to `string`, so
every pattern function is Lua 5.1's byte-based, byte-indexed one. The manual says
`mw.ustring` is "a direct reimplementation of the standard String library, except
that the methods operate on characters in UTF-8 encoded strings rather than
bytes", with its own pattern engine. The NUL bug is the first symptom the corpus
reached, not the whole problem: `%a`/`%w`/`%s` are ASCII where Scribunto's are
Unicode categories, and every index is a byte offset where Scribunto's is a
codepoint.

**What has landed so far.** The parts that need no pattern dialect and can be
checked without the service:

- `isutf8` (strict decoding, so overlong forms and surrogates are rejected),
  the four normalization forms, and `byteoffset`.
- The Unicode class table in `rustoid-core/src/lua/ustring/classes.rs`.

`byteoffset` is worth noting because the manual's definition is not a plain index
lookup: `l == 1` is the character starting *at or after* byte `i`, `l == 0` the
one starting *at or before* it, and other `l` are relative to those. An
implementation that treats `l == 0` as exclusive passes every case except the one
where `i` lands exactly on a character boundary — which is why the tests cover
`l` in `{-1, 0, 1, 2}` rather than just `0..=1`.

**Verified against live, in one batch.** Each of these returned the whole value
from `mw.ustring.match`, which means the class matched it:

| call | result | establishes |
|---|---|---|
| `match('héllo', '%a+')` | `héllo` | `%a` is a Unicode Letter |
| `match('１２３', '%d+')` | `１２３` | `%d` is Decimal_Number |
| `match('１２３', '%x+')` | `１２３` | `%x` includes fullwidth hex |
| `match('１２３', '%w+')` | `１２３` | `%w` is Letter|Decimal_Number |
| `match('　', '%s')` | U+3000 | `%s` includes Separator |
| `match('x', '%s')` | no match | `%s` is not ASCII-only |
| `match('Ａ', '%u')` | `Ａ` | `%u` is Uppercase_Letter |
| `match('ａ', '%l')` | `ａ` | `%l` is Lowercase_Letter |

The `%x` case is the one that would have been easy to get wrong: reading the
manual's "adds fullwidth character versions of the hex digits" as describing
ASCII hex digits would reject `３`, and the service accepts it.

**Still to do.** The pattern engine itself is now [`pattern.rs`], a port of
`lstrlib.c` over codepoints. It needs `%b`, `%f`, captures, back-references and
position captures, and all of those are in and tested; cached modules call the
four pattern functions 722 times against 134 calls to the already-correct `sub`,
so this was unambiguously the priority.

Two decisions are deliberately deferred until they can be measured rather than
guessed:

- `%c` vs the Cc category boundary, and `%g`'s exact printable set.
- `maxStringLength`: the manual gives no value, and modules guard against it, so
  an invented constant would be worse than a clear failure. `maxPatternLength`
  is documented as 10000 and can be added whenever it is needed.

`mw.ustring.format` is called 41 times and remains a pass-through to
`string.format`. That is deliberate: every cached call site applies `%s`/`%i`/`%d`
with no width or precision to non-ASCII text, where the two agree, so changing it
would be churn without an observable difference.

## The pattern engine, and what it moved

The matcher is a port of `lstrlib.c` rather than a design from the manual,
because the manual does not spell out the corners that decide real output:
greedy-vs-minimal backtracking order, the `%f` frontier rule, and how an empty
capture reports a position. Those are settled by the C, and "looks equivalent"
diverges on real patterns.

The port is over **codepoints**, which is the observable difference from
`string`. Verified against the service: `mw.ustring.find('héllo', 'l')` returns
`3`, not byte 4, and `gsub('a1b2', '(%a)(%d)', '%2%1')` returns `1a2b`. Both match.

Three behaviours were pinned by measurement rather than reasoning, and one of
them corrected a real bug in the first draft:

- **`gsub('abc', '%a', '%1')` is `abc`, not an error.** With no captures, `%1` in
the replacement refers to the whole match. The draft raised "invalid capture
index", which cost 36 pages. `gsub('abc', '%a', '%0%0')` is `aabbcc`, and
`%2` with one capture *does* raise — so the error is reserved for an index out of
range given that a captureless pattern has one implicit capture.
- **`%f[%a]%a+` on `xword` is `xword`.** The frontier matches at position 0
  because the previous character is out of range and reads as `\0`, which is not
  a letter. My first expectation (`word`) was wrong.
- **`%a(.-)%a` on `aXbXc` matches `aX`.** Minimal expansion takes the first
  viable second letter; greedy would take `aXbXc`.

Two Lua 5.1 behaviours are deliberately *not* reproduced, because the service
does not have them:

- a **NUL byte inside a set** is an ordinary character. Lua 5.1's `classend`
  scans until `*p == '\0'` and raises "malformed pattern (missing ']')" — that is
  the `Module:Citation/CS1` bug — while the service returns a normal result.
  `nul_in_a_set_is_ordinary` is the regression guard, and it fails against a
  literal transcription of the C.
- a NUL in the *subject* is likewise a character, not a terminator.

### Scoreboard: the 23-page entry is gone

```
lua failures: 48 entries / 17 distinct  ->  26 entries / 17 distinct
output: rustoid 162291287 bytes (2.52x) -> 172428106 bytes (2.68x)
```

The `Module:Citation/CS1:832 malformed pattern` entry — 23 pages, the largest in
the table — is **eliminated**, and no new failure took its place. `Module:Hatnote
inline` fell from 2 pages to 1.

The ratio going *up* is the honest reading of that: modules that previously
aborted on a pattern error now run to completion and emit more output. That is
progress towards parity but it is not parity — the pages are now producing
plausible-looking content that still differs from Parsoid, which is a less
visible failure mode than an error message. The score is still `0/46` and the
next entries are all *value* differences rather than API gaps
(`Module:Main list:28` formatting nil, 7 pages).

## `__pairs`/`__ipairs`, and the `Module:Arguments` proxy

`Module:Main list:28` reported `bad argument #2 to 'format' (string expected, got
nil)` on seven pages. The line is `string.format('For a more comprehensive list,
see %s.', pages)` with `pages = mHatlist.andList(args, true)`, so the question was
why `andList` returned nil — four modules away.

Following the chain to its end found the real bug, and it is not in any of those
modules:

```
Module:Main list      andList returns nil
Module:Hatnote list   stringifyList returns nil because #list == 0
Module:TableTools     compressSparseArray returns {} because pairs(args) is empty
Module:Arguments      args is an *empty proxy table*; its contents exist only
                      behind __pairs
```

`getArgs` returns `setmetatable({}, metatable)` — literally an empty table. Every
argument lives in `metaArgs` and is reachable only through `__index`, `__pairs`
and `__ipairs`. rustoid had **no `__pairs`/`__ipairs` support at all**, so `pairs`
on that table yielded nothing, everything downstream saw an empty arguments table,
and `string.format` got nil.

The manual's own list of Scribunto's differences from stock Lua settles that this
is part of the contract:

> `ipairs()`: Support for the `__pairs` and `__ipairs` metamethods (added in Lua
> 5.2) **has been added**.

They are 5.2 features back-ported into Scribunto's 5.1 runtime, so a faithful port
needs them. Both are implemented by wrapping the `pairs` and `ipairs` globals in
Lua rather than by changing the runtime: the raw iterators are captured first (so
the wrappers cannot recurse into themselves), `__pairs` is read through the
metatable so an inherited one counts, and a table without the metamethod behaves
exactly as before.

This was worth more than the seven pages: `Module:Arguments` is the shared front
door for template arguments across the wiki, so any module that reads its
arguments through it was degraded, silently, into seeing none.

### Scoreboard

```
lua failures: 26 entries  ->  25 entries
output: rustoid 172428106 bytes  ->  171306922 bytes (2.66x)
```

One entry left the table (`Module:Main list`, 7 pages) and one entered it
(`Module:Subject bar`, 6 pages). The entering one is **not** a regression, and
this was checked rather than assumed: with the fix reverted, `Subject bar` fails
ever more with `Module:Sister project links/config was not preloaded` — it used to
die at the `pairs` step and now runs far enough to reach a defect of its own at
line 18. The distinct-failure count is unchanged at 17, so the two swapped places
rather than accumulating.

The next entry is `Module:Subject bar:18`, where `tonumber(mw.ustring.match(k,
pattern))` receives nil for a key the pattern does not match. Worth noting for
whoever picks it up: the service renders the same input cleanly, so something
differs in what `pairs` yields there, and the module itself looks unable to
survive a non-matching key — which makes this a question about the argument table
rather than about `tonumber`.

## Returning one `nil`, not no values

`Module:Subject bar:18` reported `bad argument #1 to 'tonumber' (value expected)` on
six pages. The line is

```lua
local ord = tonumber(mw.ustring.match(k, pattern))
```

and the cause was **arity, not a missing match**. A failed `mw.ustring.match`
returned *zero* values, so `tonumber` was called with no argument at all.

Lua's own pattern functions return exactly one nil on a miss
(`lstrlib.c`: `lua_pushnil(L); return 1;`), and the difference is observable
because a function given no argument behaves differently from one given an
explicit nil: `tonumber()` raises "value expected" where `tonumber(nil)` returns
nil. `find`, `match` and the anchored-captures primitive now all return one nil.

The module was again fine and the runtime was wrong — its own `if ord then` guard
is exactly the right code for a nil match, and the service renders the input
cleanly.

Two things were checked rather than assumed while tracing it, and both ruled out
theory that looked plausible:

- **`tonumber(nil)` returns nil; it does not raise.** Every form tested agrees
  (`tonumber(nil)`, a nil variable, a function returning nil). Only `tonumber()`
  with *no argument* raises, which is what made the arity the whole story rather
  than a coercion gap.
- **`select(2, x)` returning no values for a single-valued `x` is stock Lua**, not
  a rustoid bug — confirmed against the `lua` binary, where
  `select('#', select(2, one_nil()))` is 0. It was therefore rejected as evidence
  twice, and an assertion built on it was rewritten rather than the engine being
  "fixed" to match a wrong expectation.

### Scoreboard

```
lua failures: 25 entries / 17 distinct  ->  19 entries / 16 distinct
output: rustoid 171306922 bytes  ->  171099151 bytes (2.66x)
```

`Module:Subject bar` (6 pages) left the table. One entry entered it
(`Module:Wikt-lang:213`, 1 page), and it is not caused by this change: it was
already present in an earlier run's table. `Wikt-lang:213` is
`parent.args[1] and parent.args or frame.args` with `parent` nil, i.e. a
`getParent()` question rather than a pattern one, and it is the next thing to
look at after the remaining entries.

The remaining entries are now mostly one or two pages each, and several are
*missing data or API* rather than value differences:

| entry | kind |
|---|---|
| `Module:Hatnote inline:16` `newChild` | missing `mw.html` method |
| `Module:Music chart:1736` `loadJsonData` | missing API |
| `Module:Location map:620` `data/Pacific Ocean` | module not fetched |
| `Module:Multiple image:177` nil arithmetic | value difference |
| `Module:Math:100` `random` empty interval | API behaviour |

## Missing APIs, and the shape of what is left

The failure table's top entries stopped being engine defects and became missing
functionality. Working through them cleared most of the table, and two of the
fixes were worth more than their page counts.

| added | why it was blocking |
|---|---|
| `frame:newChild{}` | `Module:Hatnote inline` builds a frame for `Module:Hatnote` through it |
| `mw.loadJsonData` | `Module:Music chart` loads its chart data from `Module:Music chart/%s.json` |
| `mw.text.jsonDecode` | the same JSON bridge, with both documented flags |
| `mw.ext.data.get` shape | `Module:NUMBEROF/data` walks `schema.fields` |

### `frame:getParent()` for a direct invoke — the one that mattered

`Module:Check for unknown parameters:195` opens with `frame:getParent().args` and
failed on it. The cause was that `has_parent` was keyed on the **Template
namespace**, so a call inside a template got a parent while a direct `{{#invoke:}}`
from an article did not.

The manual contradicts that twice over, most directly:

> it returns the frame for the page that called `{{#invoke:}}`. … This remains
> true regardless of whether this function is called directly from the "main"
> module invoked by `{{#invoke:}}` or from library module code accessed via
> `require()`.

Only the debug console and `mw.loadData` see nil, and a parent's own
`getParent()` is nil because there is no access to a grandparent frame.

The namespace test is the interesting part: it looked plausible, and it made the
*common* case pass. The defect only appeared on direct invocation, which is why it
survived so long. An existing test asserted the nil behaviour and had to be
inverted — it encoded the bug.

### Return arity, again

The `mw.ustring` pattern functions returned *zero* values on a failed match where
Lua returns exactly one nil (`lstrlib.c`: `lua_pushnil(L); return 1;`). The arity
is observable because a function given no argument differs from one given an
explicit nil, so `tonumber(mw.ustring.match(k, p))` — which `Module:Subject bar:18`
writes — raised "value expected" on every non-matching key.

Adding `mw.text.jsonDecode` exposed the mirror image of the same lesson: the
`pcall`-swallowed-missing-page case never reached the retry loop, because
`run_once` only consulted the missing-module list on the *error* path. A module
that wraps the load in `pcall` — which `Module:Music chart` does, substituting a
red error span — "succeeded" with the data missing and the retry never happened.
The success arm consults the list now.

### Anchor handling in `gsub` and `gmatch`

Three defects in the anchored-pattern plumbing, each confirmed against the `lua`
binary rather than reasoned about:

- **`gsub` did not stop after one substitution for an anchored pattern.** Lua's
  `str_gsub` ends its first iteration with `if (anchor) break;`, so
  `gsub('abc', '^(%a)', '<%1>')` is `<a>bc`. The loop re-anchored at every
  position and produced `<a><b><c>`.
- **`gmatch` stripped a leading `^`**, inventing matches: `gmatch_aux` passes the
  pattern straight to the matcher, where `^` is an ordinary caret, so
  `gmatch('abc', '^%a')` yields nothing where stripping yielded three.
- **The anchored-captures primitive prefixed `^` unconditionally**, so an
  already-anchored pattern became `^^(a)` — the second caret being literal — and
  matched almost nothing, making `gsub` silently return its input.

### Scoreboard

```
lua failures: 17 entries / 15 distinct  ->  9 entries / 8 distinct
output: rustoid 171567479 bytes (2.66x)  ->  164242048 bytes (2.55x)
```

Best ratio so far, and no new entries appeared across four consecutive runs. What
remains is mostly *absent data or absent extensions* rather than defects:

| entry | kind |
|---|---|
| `Module:Music chart/album.json was not preloaded` | fetch loop does not reach it offline |
| `Module:Wikidata label was not preloaded` | same |
| `Module:Location map/data/Pacific Ocean` | missing data module |
| `Module:Piechart` `parseMetaParams` (2 pages) | one module's own input handling, not yet diagnosed |
| `function not found: main` | one page, not yet diagnosed |
| `Module:Math:100` `random` empty interval | value difference / API behaviour |
| `Module:Math:363` `log10` of a string | value difference |
| `Module:Multiple image:177` nil arithmetic | value difference |

The headline score is still `0/46` and 45/48 pages still carry literal `{{...}}`,
so none of this has yet converted into a byte-identical page. The failures are the
blocking errors; what remains after them is value parity, which is a different and
less tractable kind of work — the pages must now be compared rather than
debugged.

### A method note

Four times in this stretch, measurement overturned what reading suggested, and
coding to the reading would have produced a wrong fix or a test that encoded the
wrong behaviour:

- `tonumber(nil)` does **not** raise; only `tonumber()` with no argument does.
- `select(2, x)` returning no values is stock Lua, not a bug.
- `(a)(x)?(b)` does **not** match `ab` — a `?` on a capture cannot match empty.
- `mw.ustring.isutf8`, `tostring(select(...))` and `find`'s result were each
  ambiguous as an oracle and were replaced rather than trusted.

The `lua` binary on this machine is 5.5, so it is a reference for *semantics that
5.5 did not change* rather than for everything; where 5.1 differs, `lstrlib.c`
from the vendored source is the authority, and it settled `gsub`'s anchor rule.

## A cache that can be destroyed by a legitimate-looking run

A full online corpus run — the obvious thing to do when the cache is missing a
data module — rewrote **every** `html:` entry at the wiki's latest revision. The
pinned revisions the corpus was written against are not recoverable from
the cache afterwards, so six pages went from "compares and differs" to "cannot be
compared at all", and the scoreboard's denominator silently changed from 46 to
42. That is the worst failure mode available here: the run *looks* like it
worked, and the number it prints is not comparable to the previous one.

Two independent ways to lose a baseline, and the second is the one that fired:

- `compare_page` writes `Rendered` at the corpus's pinned revision, which cannot
  orphan anything.
- `fetch_from`, the auxiliary wikitext path, asked `latest_revid` and stored the
  answer under the page's own `html:` key. It had no reader for rendered HTML —
  every caller wants a page's *source* — so it fetched the page as if it were a
template and put the result where the baseline lived.

The guard is now in the writer rather than in the callers: `put` refuses to
replace a `Rendered` entry with a revision it does not already hold, whatever the
caller says, and the auxiliary path refuses `Rendered` outright. The refusal is a
trace rather than an error, because aborting a page over a *declined
replacement* would be worse than serving the body it already had. `--refresh`
still re-pins, by removing the entry first.

The guard compares against the **existing** entry's revision, not the incoming
body's. That is not a detail: the body that caused the damage stated no revision
at all (`revid: None`), so a guard keyed on the incoming `revid` — which is what
the first draft did — would have returned `false` and let the exact failure it
exists to prevent straight through.

### The pin now lives in the corpus, not only in the manifest

`ONLINE-PARITY.md` already records that the manifest is the only file whose loss
is unrecoverable. That is true of the revision too, and it had a consequence the
reindex work did not cover: `--reindex` could serve every body and still report
every page as skipped, because the revision lived *only* in the index. Writing
the corpus by hand had the same gap — a pinned corpus that cannot state its
pins.

A corpus entry now carries its revision: `Title @ revid | tags`. The built-in
corpus pins all 48 entries, and the six whose cached revision had been
overwritten were re-pinned to what is on disk rather than re-downloaded. An
entry whose revision is gone reports as skipped, because comparing another
revision quietly is exactly the failure the pin prevents. `Zebra` is the single
entry with no pin — its body predates the manifest entry — and a test names it
rather than tolerating a silent absence.

A malformed revision is a corpus *error*, not a dropped pin. An unpinned entry
still compares, so a typo would look like success, which is the same trap as the
manifest gap one level up.

### Scoreboard, with the denominator restored

```
before: 0/42 compared, 6 skipped   (cache damage; not comparable)
after:  0/46 compared, 0 skipped   (pinned; jsonEncode fixed)
output: 64473554 bytes parsoid, 164849267 rustoid (2.56x)
```

The two stalled pages are `Cristiano Ronaldo` and `Lionel Messi`, the corpus's
densest Lua consumers. They are reported as `stalled` rather than as failures,
which is what the per-page cap is for; the previous run had them reaching a
`transclusion` difference, so the next thing to check is whether this is a
regression or the cap doing its job on a slower run.

## `mw.text.jsonEncode`, and what an encoder port actually has to reproduce

`Module:Owidslider:88` and `Module:Piechart` both encode a config table to JSON,
and the call was a nil field, so the module errored instead of rendering.

The interesting part is **not** the JSON, it is Scribunto's decision about the
value, and reproducing that decision is why this is a port rather than a call to
`serde_json`:

- **Array or object.** A Lua table is a JSON *array* when its keys are exactly
  `1..n` in order, or `0..n-1` under `JSON_PRESERVE_KEYS`; otherwise an *object*.
  So `{}` is `[]`, and a table with a hole is an object. PHP's `reindexArrays`
  decides this by walking the keys in sorted order and requiring each to be the
  next index — and that sort is *only* for the reindex path. Sorting
  unconditionally, which the first draft did, reorders an object's keys, which
  PHP never does.
- **A digit-string key is an index when encoding.** `{['1']='x'}` is `["x"]`.
  The PHP has an explicit `ctype_digit` arm for this, and the decode direction
deliberately does not mirror it.
- **`ALL_OK` is not `serde_json`'s default.** `FormatJson::encode` is called with
  `JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE`, so `/` and non-ASCII stay
  literal where `serde_json` escapes both. Every module that encodes a URL or a
  non-Latin name would have been wrong by a byte per character.
- **The checks are Lua-level.** `checkForJsonEncode` walks the value and raises
  at the *caller's* level (`lvl = 3`, incrementing per depth), so the error names
  the module line that passed the bad value. It is reproduced in
  `LUA_STDLIB_EXTRAS` for that reason, and every message is asserted verbatim
  because a module's `pcall` may branch on it.
- **Key order is Lua's iteration order.** PHP receives the table already ordered
  by Lua's `pairs` and `json_encode` preserves it, so the observed order is
  neither the module's insertion order nor sorted — it is a function of Lua
  5.1's string hash and table layout. Verified against a live payload: the
  observed order was `startingView, caption, loop, …` against an insertion order
  of `loop, start, startingView, …`. **This is not reproducible without
  reimplementing Lua's table layout**, and is recorded as an irreducible
  difference rather than approximated. The encoder deliberately does not sort,
  since sorting would be a second, different wrong answer — it preserves
  iteration order, which is at least the order PHP would have received, even
  though the iteration itself is Rust-side mlua's rather than Lua 5.1's.

**A test encoded a wrong reading, and the implementation was right.** The first
draft asserted that a `math.huge` table key encodes as PHP's
`-9223372036854775808` key, reasoning from PHP's array-key coercion. It does
not: Scribunto's *Lua-level* check rejects an infinite key first (`Cannot use
'inf' as a table key`), so PHP's coercion never sees it. The test now asserts the
error. This is the fifth time in this work that a reading was overturned by
measurement, and the pattern is the same each time — the reading of *one* layer
is right and the layer above it already handled the case.

### The oracle for this one was on disk

The `Polio vaccine` Parsoid HTML already in the cache carries the Owidslider
payload the wiki actually served: a one-element wrapper array, booleans,
integral numbers, empty strings, a value containing `[[File:…|…]]`. That is a
stronger check than any hand-written expectation, and the test reproduces the
payload rather than a paraphrase of it. It pins presence, types, and escaping
without asserting key order, which the paragraph above explains cannot be
asserted.

## A frame argument resolved in only one direction

Fixing `jsonEncode` did not clear the `Module:Piechart` entry — it moved it one
step *down* the module, from `jsonEncode is nil` to
`Module:Piechart:782: attempt to index local 's' (a nil value)`. That is the
shape of progress worth recognising: the failure advanced rather than
disappeared, which is what fixing a barrier looks like. The scoreboard went from
9 failures / 8 distinct to 8 / 7, and the entry's *name* changed.

The line is `priv.trim(frame.args[1])`. Isolating it took six one-line calls:

| wikitext | before | after |
|---|---|---|
| `{{#invoke:M|f\|1=LIT}}` | `nil` | `LIT` |
| `{{#invoke:M|f\|1={{#if:yes\|A\|B}}}}` | `nil` | `A` |
| `{{#invoke:M|f\|1={{#invoke:M\|f\|N}}}}` | `nil` | `N` |
| `{{#invoke:M|f\|X}}` (positional) | `X` | `X` |

So the construct was never the problem. **A plain literal named argument failed
exactly as a nested `#invoke` did**, which ruled out expansion entirely and
pointed at the argument table. The asymmetry in the last row was the whole clue:
positional worked, named did not.

The args metatable is supposed to resolve `args[1]` and `args["1"]` to each
other, and its comment said so — but it only ever redirected *toward* the
numeric key. A positional argument is stored as `t[1]`, so `t["1"]` found it
through the metatable; a named argument is stored as `t["1"]`, so `t[1]` looked
up a numeric key that was never there and got `nil`. One direction worked and the
other did not, and nothing exercised the second until a template was written that
way — `Template:Pie chart` passes its data as `|1=…`.

This is why the error was so far from its cause: the module read `nil` from an
argument table that *had the value*, indexed the nil, and reported
`attempt to index local 's'` with no mention of arguments or of the invoke. The
`data-mw` on the same span showed the argument present and correct, which is what
excluded the expansion theory.

The metatable now tries the caller's spelling and then its counterpart, and a
miss on both is nil rather than an error. The guard test was verified to fail
with the old single-direction lookup restored — the third such verification this
session, and the second where a test would have passed against the bug if it had
only asserted one direction.

### A note on what the failure table is now measuring

The corpus's headline score has been `0/46` throughout, and it will stay 0 until a
page matches byte-for-byte. The secondary count is the live measure, and it has
become a *tail of one-offs*: after both fixes, 8 entries across 7 distinct,
of which

- 1 is a missing cached data page (`Module:Location map/data/Pacific Ocean`),
- 1 is a missing cached JSON page (`Module:Music chart/album.json`),
- 1 is a module that does not export `main`,
- 2 are `Module:Math` value differences,
- 1 is `Module:Multiple image` nil arithmetic,

and none is a missing API. The remaining work is therefore *value* parity —
expanding correctly and then rendering identically — which the failure list can
no longer rank. The unexpanded-wikitext counts are the next instrument: 45/48
pages still carry literal `{{…}}`, so the question is now which of those is a
missing template, a missing module, and a genuine expansion defect.

### A parser-function branch loses a transclusion — known, reduced, not fixed

After both Lua fixes, the `Module:Piechart` entry moved *again*, to
`Module:Piechart:213: piechart data is empty`. That is inside `renderPie`, on
`if #json_data < 2`, so the argument arrives and is a string — but an empty one.

Reducing it to six one-line calls gives a clean division:

```
{{#invoke:M|f|1={{#if:yes|LITERAL}}}}                    -> LITERAL
template arg  {{#if:{{{v|}}}|{{#invoke:M|f}}}}           -> ""
{{#invoke:M|f|1={{#if:yes|{{#invoke:M|f|N}}}}}}          -> ""
{{#invoke:M|f|1={{#if:yes|{{Tpl}}}}}}                    -> ""
positional    {{#if:yes|{{#invoke:M|f|N}}}}              -> ""
{{#invoke:M|f|{{#invoke:M|f|N}}}}                        -> N   (no #if)
```

So the trigger is **not** the nested invoke, **not** the named argument, and
**not** the template parameter: a parser function whose chosen branch holds a
*transclusion of any kind* yields an empty string, whichever spelling reaches
it. A literal branch is fine.

The cause is visible in `ParserFunctions::expand_kv`
(`pipeline/parser_functions.rs`): a branch that arrived as `KeyValue::Str`
returns that string, but a branch that arrived as `KeyValue::Tokens` returns
those tokens **verbatim** — and those tokens were captured before expansion, so
they still hold the unexpanded transclusion. `handle_template` passes the
result straight to `encap_tokens`, so nothing expands them afterwards.

The `#ifeq` case already has a fixture guarding the token path
(`test_pf_ifeq_branch_keeps_markup_tokens`), which is what makes this a
*division* to respect rather than a switch to flip: markup tokens must survive as
tokens, transclusion tokens must be expanded. Stringifying everything would fix
this and break that.

Not attempted here because it sits exactly on the token/string boundary the 896
fixtures bind tightly — `REMAINING-BUCKETS.md` records a previous attempt at a
neighbouring problem (`pf_tag` token splicing) regressing 821→812 — so it wants
a run with room to bisect, not the tail of a session. It is the largest known
remaining defect: it is on the path of every `Template:Pie chart`, and any
template that guards a transclusion with `#if`.

## Three defects found by fetching one missing module

`Module:Location map/data/Pacific Ocean` was absent from the cache, and the
corpus reported it as a hard error on `International Space Station`. One
targeted API request and a hand-written index entry fixed that — the guard now
makes an online fetch safe, and the module is small enough that fetching it was
cheaper than reasoning about whether it could be avoided.

But fetching it changed **nothing**: the error was still reported on the next
run. That is the useful part of this section. The module was on disk and
served, and the failure was the *module's own* message naming a page that
existed — which is the kind of error that looks like a data gap and is not. The
cause is in `Module:Location map`:

```lua
local moduletitle = mw.title.new('Module:Location map/data/' .. map)
...
elseif moduletitle.exists then
    local mapData = mw.loadData('Module:Location map/data/' .. map)
else
    error('Unable to find the specified location map definition: … does not exist')
```

The name is built at runtime, so the static title scan cannot see it and the
fact was never preloaded — so `exists` read **false** for a page that was in the
cache. The module took its "does not exist" branch, `mw.loadData` was never
reached, and the retry signal that `require`/`loadData` already raise never
fired, because there was nothing to load.

`exists` now distinguishes **unknown** from **absent**: a title the host was
never asked about is recorded on its own channel and the retry loop preloads the
fact and re-runs. The channel is separate from the missing-module one because
the remedy differs — a module goes into the Lua registry, while this is only a
fact about a page and may be in any namespace, so putting it in the registry
would make `require` succeed with wikitext as the module body. A title that is
fetched and *still* absent is recorded as unfetchable, so a genuine miss answers
`false` on the next round instead of rounding forever.

The page stopped erroring and became an ordinary *difference*, which is what
made the next two defects visible. Both are on the shortest path a page takes:

### `data-mw` written as invalid JSON

The served output differed at byte 306, and the rustoid side read:

```
data-mw='{"parts":[{"template":{…}}],"parts":}'
```

A duplicate `parts` key with an empty value — the *second* `parts`, appended by
`migrate_parts_json`, with the bracket it should have written through **removed**.
It appeared 131 times on that one page, on the leading `Template:Short
description` transclusion, and it is not a cosmetic difference: the attribute is
unparseable JSON.

The function edits the serialized `data-mw` textually rather than round-tripping
it through `serde_json`, for a good reason — `serde_json` sorts an object's keys
and `data-mw` is compared byte-for-byte, so a round trip would reorder every part
of every link. The cost of that decision is that a wrong byte offset produces
*something that is not JSON* instead of an error, and the empty-`parts` branch
had exactly that: it derived the closing bracket's position from

```rust
let close = data_mw.len() - rest.trim_start().len() - 1;
```

where `rest` had already been trimmed twice, so the offset was short by however
much whitespace had been trimmed. `{"parts":[]}` came out as
`{"parts":,"TEXT"]}`. Both brackets are now *located* in the string rather than
computed from lengths.

The tests assert the property that was missing rather than the fixed bytes —
every output parses as JSON, the entry lands at the end it was migrated to, and a
repeat migration is a no-op — because for a textual edit "valid JSON" is the
invariant, and a byte-level expectation would not have caught the original bug.
The regression was verified to fail with the old arithmetic restored, reporting
`invalid JSON {"parts":,"TEXT"]}`.

Worth noting how it was found: not by reading the function, which looks
reasonable, and not by the fixture suite, which has no test for the empty-
`parts` case. It was found by diffing *output* against the served HTML, one
page in, after the error in front of it was cleared.

**Fixing it changed nothing, and that was the more useful finding.** The same
`"parts":}` was still in the output afterwards, which proved the corruption had
a *second* source. Rule it out by disabling the function entirely and counting
the occurrences again: still 131, so `migrate_parts_json` was not involved at
all in this case. A candidate fix that changes nothing is evidence, and the
cheap experiment (comment the call out, count) was worth more than re-reading
three call sites.

The real source was `strip_data_mw_key`, which removes the `parts` entry from a
`data-mw` envelope to splice another envelope's other keys onto it. It cut at
the key's colon and searched *back* for a comma — and for the first key of an
object there is no comma, so the value was removed and the key was left behind:
`{"parts":[1,2]}` became `{"parts":}`. Two textual editors of the same
serialized form, with the same class of defect, in two different files. That
is the argument for the property-based assertion rather than a byte one: the
invariant "this output is parseable JSON" would have caught both.

## A host that cannot see a module is not a host that says it is absent

The `exists` fix above did not work, and the corpus is what said so. The page
still reported `Module:Location map/data/Pacific Ocean does not exist` even
though fetching the module had changed nothing and the `exists` rule had been
widened. ISS was fine when compared on its own and failing in a corpus run,
which is the kind of difference that is easy to misread as ordering or state.

Cutting it down to two `#invoke`s on one page reproduced it. Adding a debug
print to the `exists` arm settled it in one line:

```
DBG exists key=exists known=true is_module=true full="Module:Location map/data/Pacific Ocean"
```

`known=true` — the title's facts were *already* recorded, so there was nothing
to request. The host had answered the existence question, and answered **no**.

The reason is a kind mismatch in the fetch path. A Module-namespace title is
cached under `mod:`, because `require` and `mw.loadData` must be able to tell a
module from an article of the same name — but `get_page_content`, which is what
answers an existence question, looked under `page:` only. So it missed, the
caller recorded `exists: false` as a fact, and the false fact then *suppressed*
the fetch that would have corrected it. The miss was permanent.

The general shape is worth keeping: **a `DataSource` that cannot reach a page
must not be able to record that it is absent.** Reporting a failure as a fact is
worse than reporting it as a failure, because a fact is cached and a failure is
not. The namespace now decides which kind holds the content, so the common case
costs one lookup and only a miss pays for the second.

`MockDataSource` had the identical gap, and that is the part worth calling out:
a test that adds a module and then asks whether it exists got the same wrong
answer the production harness produced, so the two agreed and the bug was
invisible from inside the test suite. It is fixed in both, with a test that
asserts the negative case too — a main-namespace title that merely *spells*
`Module:Foo` must still miss, or every article would resolve to a module.

## The smallest page is the best instrument

The 48-page corpus cannot rank its own failures. `0/46` reads the same for a page
one byte off and a page that is entirely wrong, and every page diverges in the
same place — the first template — so the headline is one bug counted 48 times
with the second, third and fourth bugs invisible behind it. The cached page
sizes make the range concrete: `Cristiano Ronaldo` is 3,126 KB and
`Help:Introduction` is 23.7 KB, and the smaller one is a *different kind* of
target as well as a smaller one.

`Help:Introduction` is 4.2 KB of wikitext and 24 KB of output, with **25
templates and zero `#invoke` and zero `<ref>`**. So whatever it fails on is core
tokenizer/transclusion/serializer machinery rather than a missing extension, and
its whole diff is readable. It is already in the corpus. The natural next target
is therefore the corpus entry with the smallest parsoid output, which is why the
failure list is now ordered that way and prints the size.

### Two instruments were wrong, and both would have produced a false finding

Worth recording because both looked like results rather than like mistakes:

- **`RUSTOID_TRACE` is not the variable.** The fetch trace is gated on
  `RUSTOID_TRACE_FETCH`, so a run with the wrong name printed no request lines
  and appeared to prove that the expander never asks for anything. It asks for
  plenty. The check that caught it was asking whether the trace worked *at all*
  on a page known to fetch — cheap, and the only thing that separated "no
  requests" from "no tracing".
- **Title lookups are case-sensitive in a way the wikitext is not.** Comparing a
  page's `{{intro to single}}` against the cache reported six templates missing,
  because MediaWiki upper-cases the first letter and the cache holds
  `Template:Intro to single`. Re-checked case-insensitively, **nothing is
  missing**: the page's whole closure is cached.

The second one mattered. It was about to become the conclusion "the corpus is
measuring cache coverage, not engine defects" — a strategic claim, drawn from a
faulty check, that would have redirected the work. The correct conclusion is the
opposite and stronger: a 23 KB page with a *complete* cache still fails, so the
bug is in rustoid.

### What it actually fails on: an unexpanded template token leaks into the output

Sixteen times on this one page, rustoid emits an element that Parsoid never
emits:

```
parsoid: <meta typeof="mw:Includes/NoInclude" id="mwAw"/>
rustoid: <template ="" id="mwAw"></template>

parsoid: (the expanded Template:Clickable button)
rustoid: <template Clickable button="" ="Editing " style="width:11em; …" id="mwOQ">
```

The ids match, so it is the same node in both — the element *kind* is wrong. The
cause is visible in the tokenizer: a template call becomes a
`SelfclosingTagTk::new("template", …)`, an internal token that the expander is
supposed to consume. When it reaches the serializer unconsumed, the DOM builder
makes an element named `template` and the target and parameters are written as
attributes — which is where `<template Large="" 1="…">` comes from.

The tokenizer already documents this shape as a *bug*: `{{ {{T}} }}` used to
become "a literal `<template T="">`". Here it happens for ordinary calls,
including ones whose templates are cached and whose names are static literals,
so the trigger is not a dynamic name. The two `#invoke`-free facts above are what
makes this tractable: no Lua is involved, and the reduction needs only
wikitext.

Not diagnosed further this session. What it needs next is a way to feed *chosen*
wikitext through the real comparison path, which is the instrument this finding
argues for:

- The harness compares `page:<title>` against `html:<title>` from the cache, and
  the live transform endpoint renders arbitrary wikitext for a chosen title. So
  hand-writing those two cache entries for a scratch title gives arbitrary
  reductions through the real comparison path — including the `data-mw`,
  encapsulation and node-id stages — which a unit test over the tree builder does
  not cover.
- That is the same trick that already pays off elsewhere: the Owidslider and
  Piechart payloads were checked against Parsoid HTML that was already on disk.

## The page-context magic words were returning the empty string

Found by following the smallest page, and it is the largest single defect this
session. Every page-name magic word answered `""`:

| word | on `Help:Introduction` | rustoid was |
|---|---|---|
| `PAGENAME` | `Introduction` | `""` |
| `FULLPAGENAME` | `Help:Introduction` | `""` |
| `NAMESPACE` | `Help` | `""` |
| `TALKSPACE`, `SUBPAGENAME`, `BASEPAGENAME`, `ROOTPAGENAME`, `…E` | | `""` |

`SERVER` and `CONTENTLANGUAGE` worked, which is what made it look like a config
problem rather than a missing feature. The cause is that `variable_value` had no
page context to read them from, and its last arm was `_ => String::new()` with a
comment admitting it: "requires page context".

**An empty string is the worst possible wrong answer here**, which is why this
survived: it is not an error, it does not look like unexpanded wikitext, and a
template acts on it. The three-line reduction shows all three consequences:

```
{{PAGENAME}}                             -> ""        (should be "Sandbox")
{{#if:{{PAGENAME}}|YES|NO}}              -> "NO"      (silently, a wrong branch)
{{#invoke:String|len|{{PAGENAME}}}}      -> 0         (should be 7)
```

So the value reaches modules as an empty argument, takes the else branch of every
guard, and appears in category sort keys as nothing. It is on the path of any
template that mentions the page name, which is a large fraction of them.

The fix threads the page title into `variable_value` and answers 13 words from
it. Every expectation is **measured, not recalled**: the wiki's own
`transform/wikitext/to/html` output for `Help:Introduction`, `Template:Foo bar/baz`
and `Sandbox`, which pinned down both the subpage splits and the encoding. The
`…E` forms are `wfUrlencode` — spaces become underscores, then `rawurlencode`,
then the characters it needlessly escapes (`; @ $ ! * ( ) , / | :`) are put back.
A test asserts all 13 against those three titles.

One invariant worth stating: the main namespace has no *prefix* on any wiki, so
namespace 0 is answered `""` regardless of what a configuration says its name is.
`MockSiteConfig` calls it `Main`, which is a display label; taking it as a prefix
produced `Main:Sandbox` for `{{FULLPAGENAME}}`.

### Scoreboard

```
before: 8 failures / 7 distinct, rustoid 164.7MB (2.55x)
after:  7 failures / 6 distinct, rustoid 163.8MB (2.54x)   best ratio so far
```

The `Location map` entry leaves the table here, having been fixed two commits
earlier — it was still listed because a full corpus run had not been made since.
A wrong *value* does not always show up as a failure count, so the byte ratio is
the number that moves for this fix; the failure table moved for the other.

## The reduction harness, and an oracle that is not the byte target

`rustoid-compare --wikitext <file>` renders *chosen* wikitext and prints the
result. That is the instrument the small-page work needed: the corpus can only
compare pages the wiki happens to have, so varying one thing meant finding a
cached page that contained the construct — and the construct then arrived buried
in hundreds of kilobytes of unrelated markup. It goes through the same render
path as a corpus page, so it exercises the whole pipeline rather than one stage.

`--page` names the title the input is rendered as, because that is what
`{{PAGENAME}}` and page-scoped words resolve against; it defaults to `Sandbox`.

**The wiki's `transform` endpoint is *not* the byte-level target.** It is a
different rendering mode from the one the corpus compares against: it keeps
`data-parsoid` where `rest_v1/page/html` strips it. A byte comparison against it
reports a difference on *every* input including a bare paragraph, so the flag
prints its rendering clearly labelled as a different mode and offers no verdict.
Byte parity is the corpus's job. This is worth knowing before building on it —
I nearly reported the mismatch as the finding.

## Still unreduced, with reductions

The smallest page's first difference is now its `<noinclude>`:

```
parsoid: <meta typeof="mw:Includes/NoInclude" id="mwAw"/>
rustoid: <template ="" id="mwAw"></template>
```

Same node — the ids match — but the element is named `template` and carries an
empty-named attribute. The tokenizer is *not* at fault: `include_limits` builds
a proper `SelfclosingTagTk::new("meta", …)` with `typeof="mw:Includes/NoInclude"`.
So the token is right and the DOM element is wrong, which means the meta is being
replaced or renamed between the token stream and the tree — and the serializer's
`mw:Includes/NoInclude` arm is therefore never reached. That is the next thing to
look at, and it is the *first* difference on the page, so everything downstream
is unmeasurable until it is fixed.

Separately, reduced to one line via the harness — a pipe inside a wikilink
inside an `#invoke` argument:

```
{{#invoke:String|len|[[Category:Foo|_VALUE_{{PAGENAME}}]]}}   -> fails
The same without the {{PAGENAME}}                             -> fine
The {{PAGENAME}} without the link, or the link without the call -> fine
```

All three ingredients are needed, so it is the argument splitter's
pipe-inside-a-link tracking being confused by a nested call after the pipe.

## Three instruments that were wrong, and how each was caught

Recorded because all three looked like results, and two of them were about to
become conclusions rather than measurements:

- **`RUSTOID_TRACE` is not the variable** — it is `RUSTOID_TRACE_FETCH`. A run
  with the wrong name printed no request lines and appeared to prove that the
  expander asks for nothing. Caught by asking whether the trace worked at all on
  a page known to fetch.
- **Title lookups are case-sensitive where wikitext is not.** Comparing a page's
  `{{intro to single}}` against the cache reported six templates missing, because
  the cache holds `Template:Intro to single`. Re-checked, nothing is missing. This
  was one step from becoming the conclusion "the corpus measures cache coverage,
  not engine defects" — the opposite of the truth, and it would have redirected
  the work.
- **A `{{…}}` count over a whole rendering is meaningless.** A transclusion's
  `data-mw` attribute legitimately contains the source wikitext, so every case
  looked unexpanded. The probe now counts `{{` only outside tags, which is the
  same distinction the harness's `Unexpanded` makes — and the mis-count made a
  correctly-rendering case look broken, twice.

The pattern in all three: the instrument was cheap to write and never checked
against a case with a known answer. Each was caught by doing that, once.

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
difference categories. The fixture score (currently 876/896) remains the guard
for the shared core, so this work must not regress it.

## Three defects were one defect, and the guard could not see it

The SPTAG family — a `SelfclosingTagTk::new("template", …)` reaching the DOM
builder, which names an element after it (`<template Large="" 1="x">`) — is the
*first* difference on `Help:Introduction`, and it sat in front of every
comparison downstream. Reducing it with the harness turned up three separate
symptoms, and they are one cause:

```
{{#ifeq:1|1|{{Large|1=x}}|z}}              -> <template Large="" 1="x">
{{#if:1|X|Y}}{{PAGENAME}}                  -> #mwt1 then #mwt3
{{#ifeq:S|exclude||[[Category:{{P|d}}]]}}  -> literal text
```

A parser function returns a *branch*, and PHP hands a handler's tokens back to
the token stream, which processes them again. rustoid spliced them verbatim, so
the raw `template` token went through untouched. The same omission consumed an
`about` id: the shared prelude took one for every token and the parser-function
path then took its own, so `#mwt2` vanished. And the third case was not the
expander at all — it was the *tokenizer*, which refused to close the call; that
one is worth its own paragraph.

### The id gap was two allocators with different conventions

`Parser::new_about_id` and `attribute_expander::new_about_id` counted from 1,
while two arms of `TemplateHandler::process` counted from 0. The discarded id
had made the disagreement invisible: taking one up front and throwing it away
put `process`'s 0-based count at 1 for the first call, so `#mwt1` looked right
and only the *next* id was wrong. That is the shape to watch for — a compensating
error that hides a real one. All four call sites now share the one helper.

### `{{` inside `[[` pushes its own closer

`{{#ifeq:S|exclude||[[Category:{{P|d}}]]}}` rendered as literal text with only
its arguments parsed. The cause is in `find_template_closing`, which kept a
stack of pending closers and treated `{{` as ordinary content while a `[[` was
open — so the branch's inner `}}` hit its "a `}}` under an open `[[` means this
template can never close" arm and abandoned the whole scan.

PHP's stack is document-order, and `Grammar.pegphp`'s `broken_template` comment
says so outright: *"once you see `[[ {{` you are looking only for `}}`"*. The
push now happens whatever the closer on top is. The `}}`-under-`[[` arm stays,
because it is what keeps `{{1x|[[Foo}}` from closing across a following line's
`]]` — the two rules are not in conflict, they are the push and the failure.

### A tplarg default is wikitext, and its closer is not a brace count

`X{{{p|{{T|d}}}}}Y` came out as `X{<link Template:D>}Y`. Two bugs stacked:

- The tplarg's default was stored as raw text (`kv_str("", &default)`), so there
  was no token for the expander to act on. PHP reads both the target and each
  default with `template_param_value`, which is the same value tokenizer a
  template argument uses.
- The closer was found by counting `}` in threes, which ate the nested
  template's `}}` as part of the tplarg's `}}}` and left the closer one brace
  short — so the `{{{` was never a tplarg at all, and the leftovers re-read as a
  bogus transclusion of `Template:D`. The existing `skip_tplarg_memo` already
  did this correctly for the template scanner; the tplarg parser now uses it.
  That is the second time a hand-rolled brace counter has disagreed with the
  scanner next to it (`find_closing` vs `find_template_closing`), which is an
  argument for there being one.

`Frame::expand` runs the chunk through the pipeline, so the templatearg arm
re-walks its result too — otherwise the default's template would be a token
nothing ever expands.

### What is left, and the bug it is really about

`Help:Introduction` still differs at byte 348, and the difference is now
diagnosed rather than reduced:

```
parsoid: …editors</div>\n<meta typeof="mw:Includes/NoInclude" id="mwAw"/>…
rustoid: …editors<span about="#mwt4" typeof="mw:Transclusion"
                   data-mw={…"function":"shortdesc"…}></span></div>…
```

The `{{SHORTDESC:…}}` is inside `Template:Short description`'s `#ifeq` branch.
Its result is *empty*, so Parsoid emits nothing; rustoid wraps the emptiness in
an `mw:Transclusion` span Parsoid never emits.

The rule is `wrapTemplates = !inTemplate`, and a template body runs in a nested
pipeline (`processTemplateSource` hard-codes `'inTemplate' => true`), so
**nothing inside a template body is encapsulated**. The body's tokens are
spliced into the caller's expansion, which carries the one wrapper, and
`data-mw.parts` accumulates the nested transclusions — the cached page shows
exactly that, one `<p about="#mwt4">` whose `parts` list holds *two* templates
and the literal `</noinclude>` text between them.

rustoid cannot express this today because it expands a body's own content and
the argument values spliced into it in **one** pass, under one flag:

- a *body* token must not be wrapped (it belongs to the nested pipeline);
- an *argument value* spliced into that body must be wrapped, because PHP
  expanded it in the caller's pipeline.

The fixture suite pins both halves, which is why the obvious one-line change
fails: forcing the body pass to `inTemplate=true` removes the `SHORTDESC` span
but also turns `{{!}}` inside a body into a `<td>`, breaking
`wikiLinks.txt`'s two `T290526` tests — `[[{{T290526}}]]` expects a *piped*
link, i.e. a literal `|`. That failure is the measurement working: 876 → 875,
and the diagnosis is that `{{!}}` keys off `atTopLevel` (`TokenHandler`, from the
pipeline option `toplevel`), which is a third flag again.

So the change is a real refactor, not a patch: a `body` (a.k.a. "nested
pipeline") flag separate from `in_template` (the caller's flag, still needed for
`{{!}}` and for `mw:Param`), used for every `wrapTemplates` decision. Until that
lands, the first difference on the smallest page is understood but not crossed.

`TemplateHandler::process` and `handle_template` now take the `wrap` flag
explicitly, which is the seam this refactor needs; the call site passes
`!in_tpl`, which is today's behaviour, so the plumbing is behaviour-preserving.

### Scoreboard

```
before: 0/45 matched, rustoid 163.8MB  (2.54x)   7 failures / 6 distinct
after:  0/45 matched, rustoid 130.0MB  (2.02x)   3 stalled, lua failures 16/12
```

No page matched yet — every page's *first* difference is still somewhere — but the
byte ratio moved a fifth of the way with no new failures, and three fixes that
were invisible in a 0/45 headline (a leaked element carrying the template's whole
parameter list is expensive, which is why the ratio and not the count is what
moves) are now in. `Help:Introduction` is the smallest failure at 23 KB and the
next rung is `Quicksilver (film)` at 35 KB.

The three fixes each removed a *class*, not an instance. That is the difference
worth watching: the SPTAG leak was one bug counted 16 times on the smallest page,
and the reduction that found it was `{{#ifeq:1|1|{{Large|1=x}}|z}}`, not the page.

**And it needs `data-mw.parts` merging first.** PHP's `TemplateEncapsulator`
walks the tokens it is about to wrap and *collects* the `data-mw` of any nested
transclusion wrappers into the outer `parts` array, removing the inner markers —
which is why the cached page has one wrapper whose `parts` holds two templates.
rustoid writes only the outer template's own `TemplateInfo`, so there is nothing
to absorb a nested transclusion's `data-mw`: un-wrapping the body *without*
merging the parts would delete the nested templates' provenance rather than
relocate it. The order is therefore: merge parts, then stop wrapping bodies. A
refactor that fixes the span first would look like progress and lose data.

## Nothing inside a template body is wrapped — and the four defects behind it

The wrapTemplates diagnosis above was right, and the refactor was smaller than
feared once the two flags were separated. `body` says "this chunk is a nested
pipeline's content"; `in_template` stays the *caller's* flag, because `{{!}}`
keys off a third thing (`atTopLevel`) that the fixtures pin. Nothing in a body is
encapsulated, and `{{SHORTDESC:…}}` inside `Template:Short description` stopped
leaving an empty span behind. That was the first difference on `Help:Introduction`
*and* on every article that transcludes the template, which is nearly all of them.

The reason the refactor looked bigger than it was: only *one* call site needed
`body = true`. Every other nested pipeline (extension bodies, module output)
already passed `in_template = true`, which gives the same answer. Only the body
pass passed the caller's flag, and only at page level does that differ.

Removing that span then exposed four more defects, in a chain:

| symptom | cause |
|---|---|
| `<!-- Start tracking -->` reached the reader | a body's top-level comments are dropped (`expandTemplates` is false in that pipeline) |
| every non-article page gained tracking categories | `{{NAMESPACENUMBER}}` was unimplemented and answered `""` |
| `#switch` answered nothing for `\|2\|3\|12\|=exclude` | the fall-through group's result is an empty-name entry, and `\|12` renders an empty key too |
| `{{#ifexpr: 39>100 \| …}}` took the true branch | `#expr` had no comparison operators — it returned the left operand |

### The online oracle is core's implementation, not Parsoid's

The fourth row's fix forced a decision that is worth stating plainly. Parsoid's
native `pf_switch` is a *simplified* reimplementation with its own behaviour —
for `{{#switch:12|…}}` it looks the target up in a dict that still contains the
target, so it answers nothing — while the served HTML comes from MediaWiki's
core `ParserFunctions`. The task is byte parity with the served HTML, so core is
the authority, and `#switch` is now a port of core's `switch()`. The fixture
suite cannot arbitrate: its expectations were generated by Parsoid native, so a
case where the two disagree simply is not in it.

The same reading also produced a smaller, quieter fix: a fall-through group's
`=result` and a bare case both render an *empty key*, so the tokenizer has to
record whether the source had an `=` at all — the `src_offsets` pair
(`key.end == value.start` marks positional) is now the signal, and the
`=`-separator search no longer starts each part "at start of line", which had
read a leading `=` as a heading opener.

### Still missing after this: the merge

Dropping the wrapper inside a body means a nested transclusion's `data-mw` has
nowhere to go, and Parsoid puts it in the *outer* wrapper's `parts` (that is the
DOM pass in the earlier note). rustoid writes only the outer template's own
info, so a body's nested transclusions lose their provenance. The visible
structure is now closer — the spurious spans are gone — but the `data-mw` of a
page whose templates nest is still short of the service's. The count that moved
is the byte ratio; the entry to watch next is one transclusion per page whose
wrapper carries more than one `parts` entry, since Parsoid merges adjacent
page-level transclusions the same way it merges nested ones.

### Scoreboard

```
before: 0/45 matched, rustoid 130.0MB  (2.02x)   3 stalled, lua failures 16/12
after:  0/45 matched, rustoid  70.4MB  (1.09x)   3 stalled, lua failures 14/12
```

The ratio nearly halved — from twice Parsoid's output to within a tenth — and no
page matched, which is the right way to read it: the extra `mw:Transclusion`
spans were *volume*, and removing them did not by itself make any page correct.
`Help:Introduction` went from 2.7x to 1.13x its size and its first difference is
still the first byte after the opening div.

That residue is whitespace, and it is the next thing to look at: rustoid leaves
`<span about="#mwt1"> </span>` where the service has nothing, so a template
body's trailing space survives here and is dropped there.

## The integrated preprocessor is why nesting leaves no trace

Following `Help:Introduction`'s first difference past the whitespace produced the
finding that reorganises the rest of this work, and it also *cancels* an earlier
one. In integrated mode the service runs MediaWiki's core preprocessor before
Parsoid sees the text, and that preprocessor expands a template body's templates
and parser functions. So Parsoid's own expander only ever runs for the page's own
transclusions. Two consequences, both measurable:

- **No `about` ids for nesting.** PHP's `TemplateEncapsulator` constructor
  allocates an id per template token and silently drops the unused ones, which is
  right natively (Parsoid walks the nesting) and wrong on a page (it does not see
  the nesting at all). rustoid allocated anyway and reached `#mwt90` on
  `Help:Introduction`, where the service reaches `#mwt12`. Taking an id only when
  the expansion can be wrapped brought it to 41. An id scheme that far out cannot
  be patched downstream — every `about` in the document is off — so this was a
  hard gate, not a detail.
- **No `data-mw` parts for nesting.** The cached page's short-description wrapper
  has exactly *one* part; the `{{Main other}}`/`{{SDcat}}` calls inside that
  template are absent. So the earlier worry — that un-wrapping a body loses nested
  provenance that Parsoid merges into the outer wrapper — is unfounded *in
  integrated mode*: there are no nested parts to lose, which is why the
  body-unwrapping change moved the ratio from 2.02x to 0.92x without losing
  anything the service emits.

The merge that *is* needed is therefore only between **sibling** page-level
ranges. Parsoid's `<p about="#mwt4">` carries `parts` of three things: the
`pp-semi-indef` transclusion, the literal text `</noinclude>`, and the
`intro to single` transclusion. That is
`DOMRangeBuilder::findTopLevelNonOverlappingRanges` merging overlapping/adjacent
ranges and `recordTemplateInfo` collecting the interleaved page wikitext as
string parts (recovered through DSR). rustoid emits the first of the three as its
own `mw-empty-elt` span and stops there — and its `<p>` never appears, because the
`<p>` exists precisely because the *merged* content contains non-transparent text.

### Two more gates, both measured rather than guessed

The residue after the id fix is not mysterious any more; counting the elements
that differ names it:

```
                         service   rustoid
mw:ExpandedAttrs               0         6
<styl> templatestyles          4         9   (5 duplicates of one sheet)
max about id                  12        41
```

- **`mw:ExpandedAttrs` on body content.** The marking fires whenever an attribute
  value holds a template; inside a *body* the preprocessor has already expanded
  those, so the service never marks them. rustoid emits 6 spans the service does
  not — a direct output difference, not only an id.
- **templatestyles are deduplicated.** rustoid emits one `<style>` per *use*, so
  `Plainlist/styles.css` appears five times where the service emits each unique
  stylesheet once.

### While I was there: the code pages are not the smaller target

`Module:Math` and `Template:Infobox` looked promising — small, self-contained,
"data-mw" and "extension" failures rather than a transclusion swamp. They are not:
both differ at byte 1 with a 13x size gap, because a `Module:` page's wikitext is
rendered under Scribunto's *content model* — the source goes into a
syntaxhighlighted `<pre>`, it is never parsed as wikitext. rustoid parses it, so
its output is paragraphs of Lua. Implementing the content model is its own feature
and not a shortcut to a first passing page.

### Scoreboard

```
before this stretch: 0/45 matched, rustoid  70.4MB (1.09x)
after:               0/45 matched, rustoid  59.3MB (0.92x)   3 stalled, lua 14/12
```

rustoid is now *smaller* than Parsoid, which is the right direction after a long
stretch of emitting spurious wrappers, and the deficit is accounted for: the
missing merges (one merged wrapper's `parts` and its `<p>`), plus the elements the
gates above name. No page matched; `Help:Introduction`'s first difference moved
353 → 403 and is now the sibling-range merge.

## DedupeStyles — one `<style>` per sheet, a `<link>` for every repeat

One of the two gates named above is now closed. A faithful port of
`src/Wt2Html/DOM/Handlers/DedupeStyles.php` lives in
`pipeline/dedupe_styles.rs` and runs as the third handler of the `fixups`
traverser (`MigrateTrailingCategories,TableFixups,DedupeStyles`), i.e. right
after `table_fixups`.

The rule is small: the **first** `<style data-mw-deduplicate="TemplateStyles:r…">`
for a key is kept, and every later occurrence of the same key is replaced by

```html
<link rel="mw-deduplicated-inline-style" href="mw-data:TemplateStyles:r…"
      about="#mwtN" typeof="mw:Extension/templatestyles" data-mw='…'/>
```

with the `about`/`typeof`/`data-mw` copied **verbatim** from the node being
replaced; a duplicate that sits in a fosterable position (the parent is `table`,
`thead`, `tbody`, `tfoot`, `tr`) cannot be swapped for a sibling without
disturbing the table, so its text is emptied instead. `seen` is per document,
matching `$env->styleTagKeys`. `skipNested` turned out to be a no-op here: it
gates only the top-level document (`DOMPostProcessor.php:810`), and the pipeline
is always top-level.

### Two facts the first attempt got wrong, both from the output

- **`data-mw` on a templatestyles node is a plain attribute, not the `data_mw`
  field.** The tag is inserted through a `mw:DOMFragment` placeholder, which
  bypasses the token-stash path that moves `data-mw` into the field for ordinary
  elements. Copying only `style.data_mw` produced a `<link>` with no `data-mw` at
  all — visible immediately in the rendered output. The pass now copies the
  attribute when present and falls back to the field.
- **The `<link>` is emitted, never dropped.** PHP's `else` branch *empties* the
  style; the non-fosterable branch replaces it. Getting either backwards would
  silently delete CSS or duplicate it.

### What is fixed, and what the dedup exposed

`Help:Introduction` now emits one `<style>` per unique `data-mw-deduplicate` key
(4) plus a `<link>` for each repeat, exactly as the service does. The residual
difference is no longer "5 copies of one sheet" — it is **9 templatestyles
occurrences in rustoid against 7 in the service**, i.e. rustoid transcludes two
templatestyles tags too many, and the `data-mw` `body` field is wrong on all of
them:

```
                      service                        rustoid
Intro to single       {...,"attrs":{...}}            {...,"body":{"extsrc":""}}
Plainlist             {...,"attrs":{...}}            {...,"body":{"extsrc":""}}
Hide checkboxes       {...,"attrs":{...}}            {...,"body":{"extsrc":""}}
Module:Shortcut       {...,"body":{"extsrc":""}}   {...,"body":{"extsrc":""}}
```

`body` is present in `data-mw` iff the source tag was **not** self-closing
(`Templatestyles::wt2html`: `if ( !$selfclosing ) { $dataMw->body = [ 'extsrc' => $content ]; }`).
rustoid's `templatestyles::style_node` hardcodes `"body":{"extsrc":""}` for
every sheet, so three of the four should have no `body` at all. That is upstream
of the dedup pass and is the next gate, together with the two extra occurrences.

## Page-scope magic words were resolved against the *template*

Chasing the two extra occurrences led somewhere much more important. Every
corpus page's first difference sat at byte 294–317 — a *systematic* early
difference, not a per-page accident — and on `Quicksilver (film)` it read:

```
parsoid: <span class="mw-empty-elt" about="#mwt1"><link rel="mw:PageProp/Category"
           href="./Category:Articles_with_short_description"/><link …Wikidata/></span>
rustoid: (nothing — the two category links were missing entirely)
```

`Template:Short description` computes that category from `{{#switch:
{{NAMESPACENUMBER}}|…}}` and `Template:Dated maintenance category` guards its own
with `{{#ifexpr:{{#if:{{NAMESPACE}}|0|1}}+…}}`. On an article both must see the
**article's** namespace (0, no name) — and on rustoid they saw the *template's*
(10, `Template`). The `#switch` hit `10 = exclude`, the `#ifexpr` took the false
branch, and the categories vanished silently.

### The cause, and why the two titles were conflated

`TemplateHandler` had a single `context_title`, set to `frame.title()`. That is
correct for one job and wrong for another:

- **relative `/subpage` targets** resolve against the frame's title — the
template that wrote `/sub` — mirroring `resolveTemplateTarget( …, $this->frame->title )`;
- **page-name magic words** resolve against the *page*.

In PHP the two never meet: the variable expansion runs in the extension, against
`$env->getPageConfig()->getTitle()`, while the target resolution stays in the
template handler. rustoid passed the frame's title to both.

`Frame::root_title()` now walks the parent chain to the frame the parse started
with, and `handle_template` / `variable_value` / `call_parser_function` take the
page title separately from the frame title. The oracle agrees the root is the
page: on `Sandbox/test` — a title in a namespace where subpages are **off** —
`{{ROOTPAGENAME}}` is `Sandbox/test`, while on `Template:Sandbox/a/b` it is
`Sandbox`; that subpage-enabled distinction is a *separate* gap rustoid still has
(`split_subpage` always splits on `/`).

### `FULLROOTPAGENAME` is not in the API's magic-word table

While fixing the above I found `{{FULLROOTPAGENAME}}` resolved as a transclusion
of `Template:FULLROOTPAGENAME`. It is a real core variable — the oracle answers
`Template:Sandbox` for it on `Template:Sandbox/a/b` — but neither the cached nor
the live `siteinfo&siprop=magicwords` lists `fullrootpagename`. rustoid derives
its variable table from that list, so the name fell through to a template. A
reserved-name fallback (`reserved_page_name_variable`) now answers the page-name
family even when the API omits a member; these names are reserved, so the
fallback cannot shadow a real template.

The single-page scoreboard moved `Quicksilver (film)` from 0.76x to 0.94x of
Parsoid's size (27.1 KB → 33.7 KB against 35.9 KB); its first difference is still
byte 311, now the templated-`href` `mw:ExpandedAttrs` marking and the missing
sibling merge rather than a dropped category.

## The trailing category span is `handleRenderingTransparentEltsBetweenBlocks`

Every corpus page's first difference is the same shape — a transclusion whose
output is `[block element][category links]`, where the service emits the block
and then

```html
<span class="mw-empty-elt" about="#mwt1"><link rel="mw:PageProp/Category" href="…"/><link …/></span>
```

with **no `about` on the links**, and rustoid emitted the links bare with
`about` and `typeof="mw:ExpandedAttrs"`. The pass that builds that span is
`DOMRangeBuilder::handleRenderingTransparentEltsBetweenBlocks` (master path
`src/Wt2Html/DOM/Processors/DOMRangeBuilder.php`, not the `PP/` copy the earlier
notes pointed at — the API listing is the reliable index here). It runs as the
*
last* step of `encapsulateTemplates`, after the marker metas are gone, and it
collects a maximal run of *stashable* elements — rendering-transparent nodes
(the category/redirect links), newline-wrapping spans, `<style>` — into one
`<span class="mw-empty-elt">`, moving the run's `about` onto the span.

Three details make the difference between right and nearly-right:

- **`migrateElements` strips `about`** from everything it moves. That is why the
  served links carry only `rel`/`href`; an implementation that leaves the stamp
  on them produces an extra attribute on every category link on the page.
- **`shouldStashRenderingTransparentNodes` needs a boundary**, not just a run: the
  previous element must be `null`, a transclusion start marker, the range's first
  encapsulation wrapper, or a `div`/`table`; and the next must be `null`, carry a
  *different* `about`, or be a `div`/`table`. Without this an inline run inside a
  transclusion gets wrapped too.
- **`isStashableElt` filters metas**: only page properties other than the TOC,
  `<*include*>` markers and stripped tags are stashable. `mw:Param`, language
  variants and the indent-pre whitespace meta must stay where they are.

### What is still missing: compound ranges

The port fixed the *standalone* case — a transclusion that is only a category
link now wraps correctly, and `Quicksilver`'s second short-description category
(`{{Main other|{{SDcat|…}}}}`) stashes as it should. Its **first** category link
still does not, and the reason is the handoff's unresolved blocker, now pinned:

rustoid encapsulates **nested** transclusion ranges eagerly, innermost first. By
the time the outer range runs, the inner `{{SDcat}}` range is already a
`<span class="mw-empty-elt" typeof="mw:Transclusion">`, and a wrapper span is not
stashable, so the run `[linkA, linkB]` that PHP sees is `[linkA, span]` in
rustoid and the boundary test fails. PHP instead merges the overlapping ranges
into one *compound* range (`findTopLevelNonOverlappingRanges` +
`recordTemplateInfo`), so all the constituent parts are range-level siblings and
the run is contiguous. Until that merge exists, the trailing-run stashing can
only work on a range that has no nested transclusion inside it.

### A cache gap the work exposed

`Template:SDcat` was missing from the cache — rustoid had never fetched it,
because the NAMESPACE bug above suppressed the very branch that calls it — so
rustoid rendered a red link where the service renders a category. Fetching it
one page online wrote the body but **not** the manifest: `write_index` is only
called on the corpus path, so a single-page or `--wikitext` run leaves newly
fetched bodies unreachable offline. `--reindex` recovers them, and it only
*adds*: existing page entries keep their revisions, which is what the corpus pin
needs. Any future online population should either go through a corpus run or be
followed by a `--reindex`.

### Scoreboard

```
session start:   0/45 matched, rustoid 163.8 MB (2.54x)
after this run:  0/44 compared, 4 stalled, rustoid 49.9 MB (0.77x)
```

The corpus grew from 45 to 48 entries during the session, so the comparison count
is not directly comparable across the two runs. What *is* comparable is the first
difference, and that moved on every article page (311 → 404 on `Quicksilver`,
`Unix`; 294 → 370 on `Sundial`; 305 → 392 on `Bicycle`).

The remaining `{{#invoke:` literals on 30 of 48 pages are mostly missing modules
(`Module:Message box/configuration`, `Module:Settlement short description`,
`Module:Unsubst-infobox`, `Template:Wikidata/i18n`) and JSON modules that need
`mw.loadJsonData` preloading. `--reindex` recovered 330 bodies that earlier
single-page fetches had written without recording them in the manifest, so some
of those gaps are already closed; the rest need targeted fetches.

## `#invoke` passed its two flags swapped

Chasing why the trailing-run stash refused to fire on `Quicksilver` turned up a
second, unrelated bug on the same line. `expand_invoke` takes `wrap` (PHP's
`wrapTemplates`) and the caller's in-template flag; the call site passed
`in_template, body` — so a **page-level `#invoke` was never wrapped at all**:

```
service: <span about="#mwt1" typeof="mw:Transclusion" data-mw='{"parts":[{"template":{"target":
           {"wt":"#invoke:String","function":"invoke"},…}}]}'>3</span>
rustoid: 3
```

and an invoke *inside* a template body **was** wrapped — and a wrapper span is not
stashable, which is exactly what kept `Quicksilver`'s trailing category run from
being grouped. Passing the local `wrap` fixes both, verified against the oracle
for the page-level case and against `{{Short description|…}}` for the body case.

The *second* flag stays `body`: substituting the caller's `in_tpl` there inflated
that page from 35 KB to 135 KB by making the module's re-parsed output expand as
page-level content (a `<pre>` per template parameter, swallowing the page tail).
`body` is the value that matches the service, so the parameter is now named for
what it is rather than for what the comment guessed.

## Cite reserved the first 25 ids — and no longer does

With the category span shape right, every corpus page's first difference was the
`id` on the **first body element** — and it was not off by one, it was off by 25:

```
parsoid: <section data-mw-section-id="0" id="mwAQ">   (counter 1)
rustoid: <section data-mw-section-id="0" id="mwGg">   (counter 26)
```

A temporary trace of `assign_node_ids` showed why: 47 ids were already in the tree
when the pass started, and 25 of them were `mwAA`, `mwAQ`, `mwAg`, … `mwCA` —
assigned by the **Cite** pass. `DocIds::take` numbered Cite's spans from its own
counter that started at **0**, so the first one was `mwAA` (which the live service
never emits at all) and the allocator then skipped the whole range.

Parsoid does not pre-assign these: Cite's DOM post-processor only builds elements,
and the single document-order id pass numbers them along with everything else. So
the fix was not "start at 1" — that would only relabel the hole — but to stop
assigning ids in Cite and mark the elements for the id pass instead
(`empty_dp_slot`, the existing "takes an id but emits no `data-parsoid`"
mechanism). Every `take()` result was used directly as an `id` attribute and never
as an `href` target — the anchors use the authored `cite_note-…` ids — so nothing
else needed the value.

That is the whole change, and it is a global one: the ids on both sides now agree,
and every article page's first difference moves from byte 311 to **404**:

```
                        before   after
Quicksilver (film)        311      404
Unix                      311      404
Sundial                   294      370
Bicycle                   305      392
Megadeth                  300      382
Canada                    300      380
Israel                    -        372
Nobel Prize               317      416
Chernobyl disaster        317      414
Help:Introduction         403      403
```

## Two more findings from the first-difference sweep

A sweep of every corpus page's first difference (and its size ratio) put the
systematic gates in order, and turned up two more:

```
World War II        255 / 1.79 MB vs 2.55 MB   the run below
List of sovereign   2 / 0.98 MB vs 0.67 MB    a missing mw-empty-elt
Hydrogen            1 / 1.08 MB vs 4.36 MB    (output 4x too big)
Module:Math         1                            content-model page
Template:Infobox    6                            content-model page
Toolbar: nothing passes
```

`Template:Weather box` looks like the closest page by matching prefix (18 505
bytes) but is not: rustoid emits 23 KB against Parsoid's 352 KB, so the prefix
matches only because rustoid stops producing content. Size ratio is the better
tie-break, and it points at `Quicksilver` (0.96), `Association football` (0.95)
and `COVID-19`/`Chess` (1.03).

### The transparent run is migrated as a whole

`World War II` byte 255: `Template:No image carousel` is
`__NOMEDIAVIEWERCAROUSEL__<includeonly>[[Category:…]]</includeonly>`, so its
range is `[<meta property="mw:PageProp/nomediaviewercarousel"/>, <link …Category…>]`
— both rendering-transparent. Parsoid serves **both inside one**
`<span class="mw-empty-elt" about typeof="mw:Transclusion" data-mw>`, while
rustoid wrapped only the first and left the link beside it.

`DOMRangeBuilder::handleFirstRenderingTransparentNode` collects the whole
*contiguous run* of stashable siblings (`getNextElementSibling` while
`isStashableElt`) and migrates all of them, so the early transparent-target wrap
now does the same, gated on the same boundary test the trailing stash uses. That
is a structural match now; the residual on those bytes is the extra `id`s below.

### rustoid gives ids to nodes the service leaves bare

With the run fixed, `World War II` still differs at 255, now only because
rustoid writes `id="mwAw"` on the `<meta>` and `id="mwBA"` on the category
`<link>` where the service writes neither. The same shape appears at the end of
`Quicksilver`: rustoid serves
`<link rel="mw:PageProp/Category" href="./Category:1986_films" id="mwow"/>`,
while the service wraps a *run* of page-level closed-marker tails —
`<span class="mw-empty-elt"><link rel="mw:PageProp/Category" …/><link
rel="mw-deduplicated-inline-style" …/></span>` — with no `about` and no `id` at
all.

Two things are named by that pair:

- a **page-prop link or meta must not be metadata-bearing** in the id pass unless
  it is inside a transclusion (the service's `<link … about="#mwt2" id="mwBQ"/>`
  has both, its bare page-level one has neither). rustoid sets `data_parsoid`
  where Parsoid sets none, so `assign_node_ids` numbers them;
- there is a **page-level** transparent-run stash too: the service's span at the
  end of `Quicksilver` carries no `about`, so it is not an encapsulation target
  but a bare grouping of closed-marker tails. `stash_rendering_transparent_elts`
  only runs inside a range today, so rustoid never makes that span.

Both are left for the next session with their reductions; neither is guesswork.

### A stray `\n|` out of `Template:Short description/lowercasecheck`

The byte-2 failure on `List of sovereign states` was a rustoid output reading

```
<span about="#mwt1">\n|</span>
```

between the two tracking categories, where the service has nothing. Reduced with
the scratch-template tooling to one call — `{{Short description/lowercasecheck|none}}`
— which every article carrying `{{Short description}}` transcludes, so it was
broad, not a `List of sovereign states` quirk.

Three plausible causes were **checked and cleared** rather than assumed:
positional arguments *are* trimmed (`{{#ifeq:1|1\n|YES|NO}}` answers `YES`, page
and body alike); `#switch` fall-through grouping matches the service exactly; and a
`|` inside a `[[…|…]]` in a switch case does not split the argument list.

Further reduction to `{{#if:1|S{{Testcases other|{{red|X}}}}T|F}}` — which renders
`S…T` and then leaks the else branch as the literal text `|F` — named the real
cause, and `data-mw` confirmed it: the service records two parameters, while
rustoid recorded **one**, `S{{Testcases other|{{red|X}}}}T|F`. The tokenizer's
argument splitter had not seen the `|` between `T` and `F`.

#### The bug: a run of closing braces

`split_template_args_impl_offsets` tracked nesting with a `double_brace` and a
`triple_brace` counter and tested `}}}` **before** `}}`. For a run of four braces —
the tail of the extremely common `{{a|{{b|x}}}}` — that read the run as a `}}}`
(which saturating-subtracted from a `triple_brace` that was already 0) plus a stray
`}`, so `double_brace` was never decremented. The splitter then believed it was
still two constructs deep, and the following top-level `|` was invisible.

The fix is a stack of open constructs instead of counters: on a run of `}`, the
*innermost* open construct decides how many braces it consumes (3 for a tplarg, 2
for a template), repeatedly, and anything left over is literal. That is what
`}}}}` needs — two `}}` closers — while `}}}` still closes a tplarg.

This changed real output on several corpus pages (the `List of sovereign states`
ratio alone went 0.69 → 1.02) and the fixture guard held at 876/896 — no regression.

### `mw-empty-elt` on a paragraph of page-property links

With the pipe leak gone, `List of sovereign states` was still failing at byte 2 —
this time on the `<p>` itself, which the service serves as
`<p class="mw-empty-elt" id="mwAg">` and rustoid as a bare `<p>`. The `<p>` holds
an empty `mw:Nowiki` wrapper and a category link, so `CleanUp::isEmptyNode` should
count it empty; rustoid's `is_rendering_transparent` handled comments and non-HTML
metas but **not** SOL-transparent links, which is the one case PHP's
`WTUtils::isRenderingTransparentNode` has and the local predicate did not.

Adding it moved the page from byte 2 to **23** (the residual is the `<p>`'s own
`id`, which the service assigns and rustoid does not, plus the `mw:ExpandedAttrs`
gate below).

Note the order of discovery: this same predicate change was written first and looked
**inert** — byte-identical output on `World War II` — so it was reverted. It was
inert only because the pipe leak was putting a visible `\n|` span inside the
paragraph, so `is_empty_node` returned false for a different reason. Fixing the
tokenizer made the predicate change effective. A change that measures as a no-op is
not necessarily wrong; it can be blocked by another bug.

## The `mw:ExpandedAttrs` gate, and why it is not a small fix

Byte 404 is, on every page, the same two attributes on the same element:

```
parsoid: <link rel="mw:PageProp/Category" href="./Category:Articles_with_short_description"/>
rustoid: <link typeof="mw:ExpandedAttrs" rel="mw:PageProp/Category"
               href="./Category:Pages_with_short_description" id="mwAw"/>
```

`Template:Short description` writes its category as
`[[Category:{{{pagetype|{{pagetype|…}}}}} with short description]]`. In **native**
Parsoid that attribute value holds a template, so `AttributeExpander` marks the
token `about` + `typeof="mw:ExpandedAttrs"` (verified: `PHP`'s marking is
unconditional on `wrapTemplates`) — and that is what rustoid reproduces. In
**integrated** mode the core preprocessor has already substituted the value, so
there is no template in the attribute, `tmpDataMW` is empty, and no marking
happens. rustoid expands the value itself at attribute-expansion time, so it still
sees the template.

The tempting fix — gate the marking on `!in_template` — is not free: the same
flag drives `wrapTemplates`, which feeds the `nlTkIndex` newline decisions, so
routing the real value into `build_expanded_attrs` (the call site currently
hardcodes `false`) changes more than the marking. Gating only the marking block
is the smaller experiment, but it would diverge from native output, which the
fixture suite pins, so it needs the fixtures checked. Left for the next session
with the reduction written down: `{{Short description|X}}` on any article is the
minimal case, and it also carries a second, unrelated difference in the same
`href` — the category is `Pages_with_short_description` instead of
`Articles_with_short_description`, i.e. `Module:Pagetype` answering `Pages` where
the service answers `Articles`.

Tracing the marking showed why the gate is structural rather than a flag: rustoid
runs `expand_templates` over the whole page first (which recursively expands every
transclusion and splices each body's tokens into one stream) and only then
`expand_attributes` **once**, at the page level. By that point the body's wikilink
still carries a token-valued target, so `build_expanded_attrs` marks it with the
page's (`in_template = false`) options. PHP does not flatten: the body is a nested
pipeline, and `AttributeExpander` runs *inside* it with the body's
`inTemplate = true`. So matching integrated mode means either expanding a body's
attributes inside the body pipeline (a structural change that also moves the
`about` allocations into it) or tagging body-produced tokens so the marking can
skip them. Both are bigger than they look; the reduction stays
`{{Short description|X}}` on any article.

**Resolved** (the last section of this file): the tagging route was taken —
`TempData::synthesized`, set on a body's/module's top-level tokens and checked in
`build_expanded_attrs`. The `Short description` case above also carried a second,
unrelated difference (`Pages_…` vs `Articles_…` from `Module:Pagetype`), which by
that point had already been fixed separately.

## `Module:Pagetype`: `getCurrentTitle()` was the invoking frame

Scoreboard from the corpus run that opened the session (`/tmp/corpusC.txt`, the
full 48, offline):

```
score: 0/48 compared (0.0%)
output: parsoid 64473554 bytes, rustoid 55907419 bytes (0.87x)
```

Up from 0.77x at the end of the previous session — the byte ratio keeps moving
before any page passes, which is the point of watching it. Re-run after the title
changes below: still 0/48, rustoid 56884909 bytes (0.88x) — the category values
and the module subpages it pulled in cost about a megabyte.

The reduction recorded at the end of the previous section — `{{pagetype|plural=y}}`
answering `pages` where the service answers `articles` — is not a `Module:Pagetype`
bug at all. Traced into the module, the `or` chain that decides the page type is
fine; what is wrong is the title it is asked about:

```
cond=true | t.ns=10 | t.type=table | content=true | det=nil | ne=nil | pc=nil |
mc=nil | nsp=nil | oth=template
```

`t.ns=10` — a page in the *Template* namespace. `p._main` opens with
`title = mw.title.getCurrentTitle()`, and inside `Template:Pagetype` that returned
**`Template:Pagetype`**, not `Zebra`. `getOtherPageType` then read
`cfg.pagetypes[10]`, which is `template`.

The cause is a conflation in `LuaContext`. Scribunto keeps three titles apart:

| question | answer |
| --- | --- |
| `mw.title.getCurrentTitle()` | the *root* page (`Zebra`) |
| `frame:getTitle()` on the `#invoke` frame | the *module* (`Module:Pagetype`) |
| `frame:getParent():getTitle()` | the invoking frame (`Template:Pagetype`) |

rustoid had one field (`LuaContext::page_title`) fed with the *invoking frame's*
title and used it for both of the first two. The parser already had the root page
in `FrameContext::page_title` — it was simply not carried into the engine, and
`run_once` passed its own `page_title` argument (which the parser sets from
`frame.title()`, i.e. the template) instead.

The fix renames the field to `LuaContext::current_title`, fills it from
`FrameContext::page_title` (falling back to the old value only when the root is
unknown), and gives `execute_in`'s frame the *module* title that `create_frame`
asked for. Three answers that were one value are now three.

That the old value "worked" for `getContent()` is worth noting: `is_current` was
compared against the *same* wrong title, so `mw.title.getCurrentTitle():getContent()`
returned the page's own wikitext while claiming to be `Template:Pagetype`. The two
errors cancelled; `Module:Pagetype`, which branches on the namespace, saw through it.

## `subjectPageTitle` / `talkPageTitle` are title objects, and a subject page is itself

Pulling the thread found a second, independent bug that the first had been hiding.
`mw.title.lua` defines the derived page titles as:

```lua
if k == 'subjectPageTitle' then
    local ns = mw.site.namespaces[data.namespace].subject
    if ns.id == data.namespace then return obj end   -- the title itself
    return title.makeTitle( ns.id, data.text )
end
if k == 'talkPageTitle' then
    local ns = mw.site.namespaces[data.namespace].talk
    if not ns then return nil end
    if ns.id == data.namespace then return obj end
    return title.makeTitle( ns.id, data.text )
end
```

rustoid returned a **string** (`prefix_title(...)`), and used the namespace pairing
`ns_id + if ns_id % 2 == 1 { -1 } else { 1 }` — which for a talk namespace gives the
*subject* id, so `Talk:Zebra`'s `talkPageTitle` was `Zebra`.

The string return matters more than it looks. `Module:Pagetype` reads
`title.subjectPageTitle` and then asks the **result** for `exists`; a string has no
`exists`, so `nonExistent` reported the page as missing and the module fell through
to `cfg.otherDefault` — `page`. That is the `pages` in the reduction. With the title
returned unchanged, `exists` still answers and the chain reaches
`getOtherPageType` → `cfg.pagetypes[0]` → `article`.

Fixed by building the derived titles through the same `title_object` constructor
`subPageTitle` uses, returning the receiver itself when it is already in that
namespace. The namespace pairing moved into two helpers,
`subject_namespace_id`/`talk_namespace_id`, shaped like MediaWiki's
`MWNamespace::getSubject`/`getTalk`:

- a negative id (Special, Media) is its own subject and has no talk space;
- a talk namespace's talk page is itself;
- otherwise the pair is `2n`/`2n+1`.

Both `luafn_site_namespaces` and `NamespaceFacts` now use them, which fixed two
more divergences found on the way: `mw.site.namespaces[-1].talk` was a *table*
(so `mw.title.lua`'s `if ns.talk ~= nil` could never be false), and
`talkNsText` on a talk page was `""` instead of `"Talk"`.

The corpus confirms the two fixes on `Unix`:

```
parsoid: <link rel="mw:PageProp/Category" href="./Category:Articles_with_short_description"/>
rustoid: <link typeof="mw:ExpandedAttrs" rel="mw:PageProp/Category"
               href="./Category:Articles_with_short_description" id="mwAw"/>
```

Same `href` now; what is left is the `mw:ExpandedAttrs` gate. `Zebra`, `Unix` and
`Help:Introduction` all still break at the same byte as before, on that gate rather
than on the category name.

## `Module:Pagetype/setindex` and `/disambiguation` had to be fetched

`parseContent` loads `mw.loadData('Module:Pagetype/' .. list)` for four lists, and
two of them were not in the cache. The name is built at runtime, so the preload scan
sees only the `'Module:Pagetype/'` prefix (and enqueues the bogus `Module:Pagetype/`);
the retry loop fetches the real name — but only once the module gets far enough to
ask. Before the fix `title.exists` was false, `parseContent` returned on its first
line, and the loads never happened, which is why the gap had gone unnoticed.

## A pre-existing node-count trip on `Zebra`

`Zebra` reports `Category:Node-count_limit_exceeded_with_short_description`: the
preprocessor's node counter reaches `max_pp_node_count` (1 000 000) while expanding
`{{pagetype}}`, and the error span's text becomes the category. The service does not
trip, so rustoid is counting more expansions than MediaWiki does on this page.

Checked by stashing the title work and rebuilding: the old revision produces the
identical `Node-count_limit_exceeded_with_short_description` and the identical
first-difference byte 444 (111 686 bytes), so this is **not** a consequence of the
title changes — the two revisions differ only in the byte count (111 686 → 111 894).
Left as the next thing to look at; the limit is a per-parse counter reset in
`build_ast`.

## The `mw:ExpandedAttrs` gate and the stray ids are one bug

With the category `href` fixed, `Unix` breaks at byte 404 on the marking, and — with
the marking suppressed as an experiment — at byte 480 on two things at once:

```
parsoid: <link rel="mw:PageProp/Category" href="./Category:Articles_with_short_description"/><link ... Short_description_is_different_from_Wikidata"/>
rustoid: <link typeof="mw:ExpandedAttrs" rel="mw:PageProp/Category" href="./Category:Articles_with_short_description" id="mwAw"/><link ... Short_description_with_empty_Wikidata_description" id="mwBA"/>
```

Both the `typeof`/`about`/`data-mw` marking *and* the `id` are wrong on the same
element, and — this is the useful part — they have the same cause. The service gives
that `<link>` neither, and it gives no id to the `<div class="shortdescription">`'s
sibling links even though the div itself (`id="mwAg"`, from the same template body)
has one.

### The rule, measured

The transform endpoint is *native* mode, so it cannot answer questions about a
*template body* (a scratch `Template:` does not exist on the live wiki, and a probe
built on one silently measures a redlink — which is how the first attempt at this
table produced three false readings; the tell is `\"`-escaped quotes, i.e. a fragment
quoted inside a `data-mw`, not a rendered element). What it can answer is the
page-level case, and the cached service HTML answers the body case. Together:

| construct | service |
| --- | --- |
| `[[Category:{{tpl}}]]` on the page | **marked** |
| `{{#switch:x|x=[[Category:{{tpl}}]]}}` | **marked** |
| `{{#ifeq:x\|y\|\|[[Category:{{tpl}}]]}}` | not marked |
| `{{#if:x\|[[Category:{{tpl}}]]}}` | not marked |
| `{{#ifexpr:1\|[[Category:{{tpl}}]]}}` | not marked |
| `{{#iferror:…\|[[Category:{{tpl}}]]}}` | not marked |
| any of the above nested inside an `#if` | not marked |
| the same link inside `Template:Short description`'s body (inside its `#ifeq`) | not marked |

So it is not "in a template body" (a body's *literal* templated attribute is marked —
`Periodic table` marks 338 `td bgcolor="{{element color\|…}}"`, and `2024 Summer
Olympics` marks a navbox `th style=`) and not "inside any parser function" (`#switch`
marks). It is exactly the four conditionals that core expands with
`trim($frame->expand(...))`: `#if`, `#ifeq`, `#ifexpr`, `#iferror`.

### Why that produces *both* symptoms

`$frame->expand` returns a **string**, and the string is spliced back into the token
stream as text and re-tokenized. Re-tokenizing synthesized text is the whole story:

- the wikilink is built from text where `{{pagetype}}` is already `Articles`, so its
  target holds no template token — nothing to mark;
- the tokens carry **no source offsets**, because their source was not the page (or
  any page). With no `tsr`/`dsr` there is no `data-parsoid`, and `storeInPageBundle`
  assigns an id only when there is something to key the node by.

`#switch` returns its branch's *original* tokens (core's `RECOVER_ORIG`), so its
branch keeps both the template and the source offsets, and both symptoms vanish.

That the same page also shows `<div class="shortdescription" about="#mwt1"
typeof="mw:Transclusion" id="mwAg">` — a literal source element from the *same*
template body, with an id — is the control: the body's own source is intact; only
the text that came back out of an `#if`/`#ifeq` is synthesized.

### What the fix has to be

`#if`, `#ifeq`, `#ifexpr` and `#iferror` must return their chosen branch *expanded to
text and re-tokenized*, not as the argument's tokens. rustoid cannot do that where it
currently evaluates them: `handle_template`/`call_parser_function` are pure functions
over `Params` with no access to the expander, and the branch's templates are expanded
later by the enclosing `expand_templates` walk.

Two things follow, and they should be taken together rather than as the two flags they
look like:

- the marking sites (`build_expanded_attrs`, and the wikilink path in
  `wiki_link_render.rs`) and the id allocation (`assign_node_ids`, keyed on
  `data-parsoid`) both need the *fact* that a token was synthesized, not a
  special case each. A `TempData` flag beside `cell_attr_terminator_seen` is the
  natural carrier, but it should be set as part of re-tokenizing, not bolted onto
  the two consumers.
- the same change also explains a third difference measured while checking the
  table: `{{#if:x|{{Short description|Y}}}}` renders 4 `mw:Transclusion` occurrences
  in the service against rustoid's 2, because the branch's nested template is
  flattened into the `#if`'s single part. A flag-only patch would leave that
  difference behind, so it is worth doing the re-tokenization.

Suppressing the marking alone was measured: it moves `Unix` and `Quicksilver (film)`
from 404 to 480 and nothing else, and the experiment was reverted. It is not a
free-standing win, which is why it is written down rather than landed.

### Also visible at 480, and unrelated

The second category in the same span is `Short_description_is_different_from_Wikidata`
in the service against `Short_description_with_empty_Wikidata_description` in rustoid,
i.e. `Module:SDcat` sees no Wikidata description. That is a `mw.wikibase` gap, not
this one, and it will still be there once the marking is right.

## Landed: the text-expanding branches no longer mark their targets

The rule from the previous section is now implemented, in the shape the evidence
supports rather than the shape Parsoid has. `#if`, `#ifeq`, `#ifexpr` and `#iferror`
are recognised by `TemplateHandler::expands_branch_to_text`, and the branch they
return is flagged `TempData::in_text_branch` in `expand_template_token`'s `_` arm
(which is where parser functions are dispatched). `AttributeExpander::build_expanded_attrs`
then skips its marking block for a flagged token.

Measured on the pages whose first difference was this gate:

```
                        before  after
Unix                     404     480
Quicksilver (film)       404     480
Sundial                  370     446
Bicycle                  392     468
Megadeth                 382     458
Nobel Prize              416     492
Association football     ~390    466
Bitcoin                  ~404    480
```

Every one moves by the same ~76 bytes, which is what a gate removed uniformly
looks like; the corpus is unmoved in aggregate (0.88x both before and after).
`Help:Introduction` (403) and `Zebra` (444) are unmoved — neither is this gate.

### Why a flag and not the re-tokenization Parsoid does

The faithful implementation is "expand the branch, serialize it to wikitext,
re-tokenize". It was written first and **it does not work in rustoid**, for a reason
worth recording: `tokens_to_string` over *expanded* tokens is lossy. The branch's
`[[Category:Article_with_sd]]` came out as `[[]]`, because the target of a wikilink
token lives in its data rather than in anything the serializer emits. (The
`data-parsoid.src` fallback that `tpl_toks_to_string` uses would only give the
*unexpanded* source back, which is the problem, not the fix.)

Core can do this because its expander is text→text and the re-tokenization happens
where the string is spliced in. rustoid's expander is token→token throughout; the
only text it can produce is a token serialization, and a wikilink's target is not in
one. So the *effect* — that an attribute which still looks templated in a branch is
not treated as templated — is what the flag encodes, at the one site that depends on
it.

### The ids do *not* follow from the same flag

The remaining byte at 480 on `Unix` is still `id="mwAw"` on the category link, and
the tempting reading — "the branch was re-tokenized, so it has no source range, so
it gets no id" — is **wrong**. The probe that kills it:

```
{{#if:x|[[Foo]]}}        oracle: <a rel="mw:WikiLink" href="./Foo" about="#mwt1"
                                  typeof="mw:Transclusion" ... id="mwAw">
```

A link inside an `#if` branch *does* get an id. It gets one because it carries the
`#if`'s transclusion wrapper, i.e. `data-mw`; `storeInPageBundle` assigns an id
whenever there is something to key the node by — `data-mw` or a non-empty
`data-parsoid`. The category link in `Template:Short description` carries neither:
the wrapping lands on the enclosing `<span class="mw-empty-elt" about="#mwt1">`, and
its `data-parsoid` is *empty*, which Parsoid discards (`$discardDataParsoid` for
`IS_NEW` + `isEmpty()`) — so nothing is keyed and no id is assigned.

So the id side is a rule about **empty `data-parsoid`**, not about provenance, and it
is a different change from this one: rustoid's link carries `data-parsoid` with a
`tsr`, and `assign_node_ids` counts *presence*. Two probes to keep in mind for it:

- `[[Category:Foo]]` at page level: the service gives the link an id (it has a real
  source range); rustoid agrees but numbers the `<p>` differently — the service
  numbers the auto-inserted paragraph (`<p id="mwAg">`) and rustoid does not, which
  is a second, independent id gap in the same three bytes.
- `{{#if:x|[[Category:Foo]]}}`: the service gives the *span* the wrapper's id and the
  link none.

## The other half of the id question: a source range, not a blob

The rule above — "an id is assigned when there is something to key the node by" —
needed one correction to become usable. "Something" is **a source range**, not the
presence of a `data-parsoid` blob:

```rust
let has_source_range = node.data_parsoid.as_deref()
    .is_some_and(|json| json.contains("\"tsr\"") || json.contains("\"dsr\""));
let has_metadata = has_source_range || node.data_mw.is_some() || node.empty_dp_slot;
```

`data-mw` stays because it is the wrapper's metadata, and `empty_dp_slot` — the
`<section>` and the `<cite>` `<li>` — is the same distinction from the other side: an
element that emits no `data-parsoid` yet does take an id.

With that, `mark_in_text_branch` clears the source range (`tsr`/`dsr`/`src`/
`srcContent`) as well as setting the flag, because the reference really does produce
no source range for a branch's tokens — the *native* endpoint shows it directly:

```
{{Short description|Foo bar}}   →  <link rel="mw:PageProp/Category"
                                      href="./Category:Articles_with_short_description"/>
```

an empty `data-parsoid` where the same link written on the page has `stx`, `a`, `sa`
and `dsr`. Two mechanisms, one cause; a flag alone would have left the ids wrong and
a cleared range alone would have left the marking.

Measured, all with the fixture guard at 876/896 and the corpus at 0.88x:

```
                        before  after
Unix                     480     550
Quicksilver (film)       480     550
Sundial                  446     516
Nobel Prize              492     562
Bicycle                  468     538
Megadeth                 458     528
```

### What that leaves: the auto-inserted paragraph

`List of sovereign states` is the closest page, and it is stuck at byte 23 on one
thing:

```
parsoid: <p class="mw-empty-elt" id="mwAg"><span … id="mwAw">…
rustoid: <p class="mw-empty-elt"><span … id="mwAg">…
```

The service numbers the paragraph; rustoid does not, so every id after it is one
low. The paragraph is auto-inserted by `PWrap`, and the native endpoint shows what
Parsoid puts on it:

```
a [[Foo]] b   →  <p id="mwAg" data-parsoid='{"dsr":[0,11,0,0]}'>
```

A `dsr` over the wrapped range. rustoid's `PWrap` gives its synthesized `<p>` a bare
`DataParsoid::default()`, so under the rule above it has no source range and takes no
id. That is the fix: the synthesized paragraph needs the range it covers. It is not
a one-line change (the range has to be computed from the tokens it wraps, and it
affects every auto-inserted paragraph in the corpus, not just this one), so it is
recorded rather than guessed at.

## A module's output has no source range either

The same reasoning applies one step out. A module's output is a *string* Scribunto
built, and rustoid tokenizes it (`tokenize_wikitext_to_items`) — so the offsets its
tokens carry describe that string, not the page. `Module:SDcat`'s second category
link is the one that showed it: rustoid keyed it (`id="mwAw"`) where the service
leaves it bare, one line below `Template:Short_description`'s own link, which the
branch fix had just stopped keying.

So the tokenizer's output is stripped of its source range at the point the module's
output enters the pipeline, through the same `strip_source_ranges` the branches use.

### What not to clear: `src`

Clearing `tsr`/`dsr` is what the ids need; clearing `src` and `srcContent` too made
`Unix` take **over 60 s and render nothing** (0 bytes against 365 082). Bisected by
making only that pair conditional: with them kept, `Unix` is back at byte 550 and
the corpus at 0.87x. The loop that reads `src` has not been found; it is left in
place and recorded here because a missing `src` turning into an unbounded-looking
run is exactly the kind of latent hazard that is cheaper to know about than to
rediscover.

### Scoreboard after the three id/marking steps

```
                        start of session   now
rustoid bytes            56 883 107        56 209 784   (0.88x -> 0.87x)
Unix first difference     404               550
Quicksilver (film)        404               550
Sundial                   370               516
Bicycle                   392               538
Megadeth                  382               528
Nobel Prize               416               562
```

`List of sovereign states` is still the closest page, still blocked at byte 23 by
the auto-inserted paragraph's missing `dsr` — see above; that is the next thing.

## The auto-inserted paragraph: the blob was never written

The blocker above dissolves once the direction is right. `ComputeDSR` computes the
range for the synthesized `<p>` correctly — `compute_node_dsr`'s `p` case reaches
`Consts::$WtTagWidths['p'] = [0, 0]`, so the range is exactly `[0, 26, 0, 0]`, as
the native endpoint showed. What was missing was the *write*. The range landed in
the structured `dp` and never reached the `data-parsoid` blob, and the id pass keys
on the blob. In PHP there is one representation — a pass that sets a `DataParsoid`
makes it visible to every later pass — so the two have to be kept in step here
explicitly.

`compute_dsr::store_dsrs` now mirrors `dp.dsr` into the blob, with two exclusions:

- Nodes expanded from a **text branch** (`dp.tmp.in_text_branch`): nothing in the
  branch came from a page, so the computation would invent a range from position
  where the service keeps none.
- The synthetic **`<html>`** root. It is the serializer's document wrapper, not
  page content, and giving it a range spends the document's first counter (`mwAQ`)
  on an element the output never prints — which shifts every id on the page by one.
  This is what the first attempt did: byte 23 → 31 and no further.

Only the `dsr` key is added, so keys `dp` does not model (a wrapper's `tmp`) survive
in the blob.

## A module's output keeps its source range (the section above was wrong)

Stripping `tsr`/`dsr` from a module's output was on the reasoning that the offsets
describe the module's string rather than the page. The service disagrees, and
`List of sovereign states` shows both sides of the distinction inside one paragraph:

```
<p class="mw-empty-elt" id="mwAg">
  <span typeof="mw:Nowiki mw:Transclusion" … id="mwAw"></span>
  <link … href="./Category:Articles_with_short_description" about="#mwt1"/>           <- no id
  <link … href="./Category:Short_description_is_different_from_Wikidata" about="#mwt1" id="mwBA"/>  <- id
</p>
```

The first link is `Template:Short_description`'s own `[[Category:…]]`, reached through
a `#ifeq` branch: a *text* expansion, re-tokenized, no `tsr`, no id. The second is
`Module:SDcat`'s return value. Both are strings, but only the branch loses the range
to the re-tokenization: a module's output is a nested pipeline with its own source,
and its tokens keep the `tsr` that pipeline gave them. The service keys the node
that has one, however bogus the offset is as a *page* position — the id pass reads
presence, never the value.

So the strip is removed. The write-back above is what turns the retained `tsr` into
the key. (`src`/`srcContent` are still left alone; the note about `Unix` hanging
when they were cleared stands, even though the module strip that motivated it is
gone.)

### Scoreboard

Nine closest pages, offline. Only the target moved, because the other eight diverge
earlier for unrelated reasons:

```
                          before   after
List of sovereign states    23       418     <- now the Wikidata description
Unix                       550       550
Quicksilver (film)         550       550
Sundial                    516       516
Bicycle                    538       538
Nobel Prize                562       562
Megadeth                   528       528
Help:Introduction          403       403
Zebra                      444       444
```

Every `<p>` on every page now takes an id, and the 8 unchanged pages show their
first difference was never the paragraph's id. Fixture guard 876/896.

## The page's own Wikidata entity was evicted by the entity cap

With the id sequence right, the next difference on `List of sovereign states` was
`Module:SDcat`'s category: `Short_description_with_empty_Wikidata_description`
where the service has `Short_description_is_different_from_Wikidata`. The module
asks `mw.wikibase.getDescription( qid )` with the qid it got from
`getEntityIdForCurrentPage()`, so the *page's own* entity has to be in hand.

`preload_entities` resolved it — `get_entity_id_for_page` is a sitelink search — but
then inserted it into the same `BTreeSet` as the literals gathered from every module
in the registry, and the loop fetched `wanted.into_iter().take(MAX_ENTITIES)`. The
registry yielded more ids than the cap, and `take` is by sort order, so the one
entity that is needed on every page could be (and here was) dropped on the floor.
It is now fetched on its own, outside the cap, and skipped by the literal loop.

Two cache facts came out of chasing it, worth knowing before an offline run:

- The page→entity answers live under a `sitelink:` key in the **entity** wiki's
  index, and here that index had been reduced to two entries while the body files
  survived. Every sitelink lookup therefore missed. `rustoid-compare --wiki
  www.wikidata.org --reindex` rebuilt it (198 bodies recovered, 200 entries) — and
  for entities that is enough, because unlike an article there is no revision to
  recover.
- `entity:sitelink:List of sovereign states.txt` held `Q11750`, whose English
  description is `Wikimedia list article` — which is what makes the service's answer
  `is different from Wikidata` for a short description of `none`.

## Test fixtures that pinned the missing blob

`test_wikitext_to_html_paragraph_break` and `test_wikitext_to_html_redirect_nowiki_bail`
asserted on a literal `<p>` / `<ol><li>`. Both now carry `data-parsoid`, which is
what Parsoid native mode emits, so the assertions match the tag boundary instead.

## Two id rules inside a transclusion

With byte 477 reached, the next difference was a pair of id questions about
nodes *inside* a transclusion. They are separate rules and both had to be fixed.

### The `about` counter is only advanced where a wrapper is produced

`List of sovereign states` had the Redirect2 hatnote at `#mwt11` where the service
has `#mwt2`. Instrumenting `new_about_id` (a temporary `RUSTOID_ABOUT_DEBUG`
label at each call site) showed ids 2…10 going to `Template:Short description`'s
own expansion — `Template:Pagetype`, four `#invoke`s, `lowercasecheck`,
`First word`, `Main other` and the `SDcat` invoke.

Those all expand inside a template *body*, and PHP runs a body through a nested
pipeline with `inTemplate => true`, where the `TemplateEncapsulator` — the object
that owns the id — is never built. rustoid already carries that context as
`body` (`wrap` is `!body && !in_tpl`), but `take_id` only consulted `in_tpl`, so
each of those tokens consumed an id while producing no wrapper. `take_id` is now
`!body && !in_tpl`, i.e. exactly the condition under which it wraps, and the
sequence comes out `1 (Short description), 2 (Redirect2), 3 (templatestyles)` —
the service's.

The earlier comment in the code claimed a discarded id was load-bearing for
`{{1x|{{T}}}}`; that case has the inner call spliced in as an *argument value*,
which is already `in_tpl`, so it never distinguished the two rules. The fixture
guard is unchanged at 876/896.

### `CleanUp::markDiscardableDataParsoid`

One line below that, rustoid keyed the `Module:SDcat` category link where the
service does not. That is `markDiscardableDataParsoid`: inside an encapsulation
range only the **first** node and the **last** keep their `data-parsoid`, plus any
node without `stx` and any node in native (extension) content. The range is the
run of consecutive element siblings sharing the `about` (`WTUtils::getAboutSiblings`),
and the running `tplInfo` comes from `DOMTraverser`.

`cleanup::mark_discardable_data_parsoid` implements that, and it is called from
`build_ast` just before `assign_node_ids` — PHP runs it as the cleanup traverser's
*store* step, so every earlier pass keeps its `dp`. Only `data_parsoid` is
dropped, not `dp` and not `data-mw`: a node with `data-mw` is keyed by it whatever
`data-parsoid` says, which is why discarding cannot unkey a transclusion's head.

Two deliberate approximations, both on the conservative side: `inNativeContent`
treats *any* `mw:Extension/…` content as native (rustoid's site config has no
native/non-native tag split, so this discards less than the reference rather than
more), and a node with no `stx` is never discarded (the reference agrees — its
condition is `empty($dp->stx) || !(last || heading)`).

### Scoreboard

```
                          before   after
List of sovereign states    477      477     <- now the encapsulation head
Quicksilver (film)          577      590
Unix                        577      852
Sundial                     543      554
Nobel Prize                 589      600
Megadeth                    555      566
Bicycle                     538      538
Help:Introduction           403      403
Zebra                       444      444
rustoid bytes            3 831 607  3 760 471  (0.72x -> 0.70x)
```

What is left on the closest pages is now one shape, not a scattering:

- **The encapsulation head.** `List of sovereign states` wants
  `<span class="mw-empty-elt" about="#mwt2" typeof="mw:Transclusion">` around the
  leading templatestyles and a separate `<div about="#mwt2">`; rustoid puts the
  head on the `<div>` and, worse, leaves a `<style>` *inside* an `<a>`.
  `Help:Introduction` wants `<p about="#mwt4" typeof="mw:Transclusion">` where
  rustoid has `<span class="mw-empty-elt" about="#mwt9">`. These are
  `DOMRangeBuilder` (the stash span, and where the head lands) and the placement
  of a templatestyles a module emits from inside a link.
- **Bicycle** differs on the *category name* (`matches_Wikidata` vs
  `is_different_from_Wikidata`) — its Wikidata entity is not in the cache, so
  `mw.wikibase` sees nothing. Every other entity lookup now works.
- **Zebra** is the pre-existing node-count trip and is not a usable target.

## A module's `frame:getParent().args` can still hold unexpanded `{{{…}}}`

Chasing `Bicycle` (whose only remaining difference at byte 538 is the category
name, `matches_Wikidata` vs `is_different_from_Wikidata`) turned up a real bug with
a clear trace. `Module:SDcat` compares the local short description against
`mw.wikibase.getDescription()`, and the local one arrives through
`frame:getParent().args.sd`, because `Template:SDcat` is just
`{{#invoke:SDcat|setCat}}`. A debug print in the invoke path showed the parent
argument still raw:

```
INV frame=Template:SDcat invoke_arg="SDcat|setCat"
    params=["#invoke:SDcat=", "=setCat"] parent=["sd=None/src=\"{{{1|}}} \""]
```

So `args.sd` is not the description; the comparison sees `nil` (or the literal
`{{{1|}}}`) and answers "different" where the service answers "matches" (the local
description and Q11442's are both `pedal-driven two-wheel vehicle`).

The value never got expanded. `{{SDcat|sd={{{1|}}} }}` is written in
`Template:Short description`'s source, but it sits inside `{{Main other|…}}`'s
argument, so by the time the `#invoke` runs the chain is
`Short description → Main other → SDcat`, and `expand_invoke_args` neither expands
`templatearg` tokens nor tracks *which* frame the value was written in. MediaWiki
resolves it because the `{{{1|}}}` node carries the frame it came from; rustoid's
`Frame` has a `parent_frame` chain but the node does not record where it belongs.

Two things were checked and are *not* the cause: `mw.text.trim(…):lower()` and the
description lookup compare correctly in isolation (a scratch engine with Q11442
answers `true`), and the entity is in the cache (`entity:Q11442.txt`).

A fix has to give a lazily-expanded argument value the frame it was written in, or
expand `{{{…}}}` eagerly at the call site in the caller's frame. The second is
wrong for `{{{sd}}}`-style values (they must expand where they were written, not in
the callee), so the first is the shape to aim at.

## Two test failures that are not this work's

`cargo test --release --workspace` reports two failures, and both reproduce at the
session-start commit `aac41df` (checked in a worktree):

- `templatestyles_parsoid_test::render_matches_parsoid_for_every_cached_stylesheet`
  — environment (the `/tmp` cache layout).
- `templatestyles_test::the_lua_frame_method_reaches_the_same_handler` — the
  module's `frame:extensionTag('templatestyles', …)` emits an empty
  `mw:Transclusion` span. Pre-existing; worth a look later, and the handoff's
  note that only the first fails was incomplete.

`missing_template_loop::a_missing_template_answers_with_its_title_as_text` *was*
broken by the write-through: it took the link text as everything after the first
`'>` in the page, which was the anchor's opening tag only while no earlier
element carried `data-parsoid`. It now reads from the last `>` before `</a>`.

## Full-corpus scoreboard, offline (48 pages)

```
score: 0/46 compared, 2 stalled
output: parsoid 64 473 554 bytes, rustoid 52 427 586 (0.81x)
```

The closest pages by first difference, and what each one is:

```
       1  Hydrogen                     the About hatnote's leading templatestyles (as List, but first)
       6  Template:Infobox             a missing class="mw-empty-elt" on the invoke's wrapper span
     403  Help:Introduction            encapsulation head: <span class="mw-empty-elt"> vs <p>
     444  Zebra                        the pre-existing node-count trip
     477  List of sovereign states     encapsulation head: <span class="mw-empty-elt"> vs <div>
     518  Israel                       (not yet reduced)
     538  Bicycle                      the unexpanded {{{1|}}} in a module's parent args
     554  Sundial / Taylor Swift       (not yet reduced)
```

`Zebra` is not a usable target (node-count trip; rustoid renders 113 KB where the
service renders 560 KB).

### The two stalls are pre-existing and are a performance limit, not a regression

`United States` and `India` exceed the 60 s per-page cap. Both **also stall at the
session-start commit** (checked in a worktree), and both are CPU-bound (`user`
59.7 s), so nothing in this work caused them. They started stalling when the
Wikidata entity index was rebuilt: with entities resolvable, modules such as
`Module:Wd` do real work instead of taking their no-data fallback, and two of the
largest pages no longer finish. That is rustoid being slower than the service, not
differing from it — the 0.81x byte ratio (down from 0.87x) is mostly the freed
`data-parsoid` and the corrected id/keying, not missing content.

### "Byte 1" is not near-parity

`Hydrogen` and `Module:Math` report a first difference at byte 1, and
`Template:Infobox` at byte 6 — but rustoid renders 3.9 MB against Hydrogen's
1.07 MB and 6 KB against `Template:Infobox`'s 196 KB. The first differing byte
says where the two *start* to disagree, not how much agrees; read it next to the
size column. `List of sovereign states`, at 1001 KB against 981 KB, is still the
only page where the two are the same order of magnitude from the first byte on.

(`Module:Math` is not a fair target at all: a `Module:` page renders under
Scribunto's content model, which rustoid does not implement.)

## The encapsulation head, measured

I spent a session on the head and did not land it. What is now *known* is worth
more than the guess I started with, because two of my starting beliefs were wrong.

### What the service actually does (probe: `--wikitext` with `{{Redirect2|…}}`)

```
<span class="mw-empty-elt" about="#mwt1" typeof="mw:Transclusion" id="mwAg"
      data-parsoid='{"autoInsertedStart":true,"autoInsertedEnd":true,
                     "pi":[[…6 params…]],"dsr":[0,171,null,null]}'
      data-mw='{"parts":[{"template":{"target":{"wt":"Redirect2",…}}]}'>
  <style data-mw-deduplicate="TemplateStyles:r1368532237" …></style>
</span>
<div role="note" class="hatnote …" about="#mwt1" id="mwAw" data-parsoid='{"stx":"html"}'>…</div>
```

So the head span is a **new wrapper that exists only to hold the transclusion
markers**, and it carries the parts that need a container: the `pi` (parameter
info), the range `dsr`, the `autoInserted*` flags and `data-mw`. The block element
that follows keeps its `about` and nothing else. That is exactly
`ensureElementsInRangeAndAddAboutIds`' "Add a span wrapper to let us add about-ids
to represent the DOM range as a contiguous chain" plus `findEncapTarget` skipping
it — the two of which I had not connected: rustoid has the `isDeletableNode` half
of that function (`span_wrap_nl`) but not the wrapper half.

The minimal form of the same shape, rustoid already gets **right** — a table cell
whose whole content is `{{Legend|…}}`:

```
<td id="mwBQ"><span class="mw-empty-elt" about="#mwt1" typeof="mw:Transclusion"
  data-mw='…' id="mwBg"><style …Legend/styles.css…></style></span>
  <div class="legend" about="#mwt1" id="mwBw">…</div></td>
```

### Belief I had wrong #1: the `<style>` inside the `<a>` is not a stash failure

I read byte 477 as "the stylesheet was not stashed". It *is* stashed — in the right
place, `Module:Hatnote`'s `TemplateStyles:r1368532237`. The `r981673959` that lands
inside an `<a>` is `Template:Legend`'s, and it is the `#invoke:Format link` module
output for a link *target*, so the `<a>` genuinely has it as a child. On the real
page the stylesheet's correct home (`Template:Legend table`'s table cell) never got
one because the flow does not reach the stash there — a separate question, and the
one the byte-477 difference is actually about is the *head*.

### Belief I had wrong #2: `isStashableElt`'s DOM-fragment check is not the blocker

`is_stashable_elt` now recurses into a `mw:DOMFragment` placeholder's stashed
children and `is_rendering_transparent_node` judges a placeholder by what it stands
for, both faithful to PHP (`getDOMFragmentContents`, `placeholderTypeOf`). They are
correct and the unit tests pin them, but they are **not** what byte 477 turns on:
the placeholder that matters there has a whole hatnote in its fragment, so PHP's
strict `all(isStashableElt)` answers false too. The change is kept because it is
right and cheap, not because it moved a byte.

### The next thing to build

`ensureElementsInRangeAndAddAboutIds`' wrapper branch, i.e. in
`wrap_transclusion_children`: when the range's first content node is an element that
cannot carry content and the range is not "already contiguous at a boundary", insert
a `<span class="mw-empty-elt">` **before the first content node**, mark it
`autoInsertedStart`/`autoInsertedEnd` + `WRAPPER`, make it the head (so `pi`/`dsr`/
`data-mw` land on it rather than on the block), and leave the block with only
`about`. That is what turns `Help:Introduction`'s
`<span class="mw-empty-elt" about="#mwt2" …>` into the service's
`<p about="#mwt4" typeof="mw:Transclusion">` shape as well, since the head is then
free to be a wrapper for the whole range rather than whichever node happened to
carry `mw:Transclusion` first.

## The wrapper step has a double-wrap trap (not yet built)

An attempt at `ensureElementsInRangeAndAddAboutIds`' wrapper branch was written and
then reverted, because a unit test caught what it did to the whitespace path.
Recording the trap so the next attempt does not rediscover it.

`wrap_transclusion_children` has *two* places that create the single-space `about`
span a newline inside a range becomes: the `NodeKind::Text` arm of the content loop,
and (in PHP) `addSpanWrappers`. In rustoid the loop's `is_deletable_in_range` gate
does **not** drop a newline that sits between a transclusion marker and a
`<table>` — so the newline becomes a `" "` span there. A wrapper step that then wraps
"every node that is not an element" wraps that span again, producing
`<span about=…><span about=…> </span></span>`. Whoever builds the branch has to skip
what the first step already produced, or fuse the two steps into one.

The three unit tests written for the branch were also wrong in a way worth naming:
they asserted the *fixture's* imagined output rather than the function's actual
behaviour, so each fix was a guess at a different wrong answer. The behaviour that
settled it: a `\n` between a transclusion start marker and a table yields
`["span", "table"]` — a lone text node never survives bare in a range.

## Byte 477 was not the wrapper step: two fragment-id counters, one map

The head `<span class="mw-empty-elt">` above was real, and it was not reachable by
building the wrapper branch. What byte 477 actually turned on was a **fragment-id
collision**, and the plan above was built on a wrong reading of rustoid's own
tree.

### The wrong reading

`{{Redirect2|…}}`'s range content in rustoid was *not* `[style, div]`. A minimal
probe that removes the page around it shows the truth:

```
{{Hatnote|Test [[Main Page]].}}
rustoid: <div role="note" class="hatnote" about="#mwt1" …>Test <a href="./Main_Page">
             <style …Module:Hatnote/styles.css…></style></a>.</div>
```

Two bugs in one line, and neither is about the wrapper branch:

- the link's **label is gone** (`Main Page` was dropped), and
- the stylesheet landed **inside the `<a>`** — as its only child.

### The cause: two counters, both starting at 0

`[[Main Page]]` renders its caption through a tunnelled DOM fragment
(`render_wiki_link_with_fragment` → `dom_fragment_token`, PHP's
`addLinkAttributesAndGetContent(…, $buildDOMFragment = true)`). The id came from
`build_ast`'s token-side counter, which starts at `0`.

`<templatestyles>` — reached through `frame:extensionTag` in `Module:Hatnote` —
allocates its fragment id from `Parser::ext_next_id`, a **separate** counter that
also starts at `0`. Both fragments then went into the one map the tree builder
resolves `mw:DOMFragment` placeholders against (`fragments.extend(ext_fragments)`),
so id `0` was ambiguous: `unpack_dom_fragments` spliced the stylesheet into the
anchor and the caption text somewhere else, where it vanished.

Probing the token stream directly (`RUSTOID_DUMP_TOKENS`) made it visible in one
line: `TOK[1] … mw:dom-fragment-token data-fragment-id="0"` (the caption) and
`TOK[3] <style typeof="mw:DOMFragment" data-fragment-id="0">` (the stylesheet) —
the same id, in the same stream, for two different fragments.

### The fix: one id space, as PHP has

PHP keeps a **single** counter and a **single** map on the environment
(`Env::newFragmentId` returns `"mwf" . $this->fid++`; `Env::setDOMFragment` stores
into `$this->fragmentMap`). rustoid's split space was the deviation. The
`next_id` parameter of every token-side pass is now a shared
`&Cell<usize>`, and `build_ast` passes the parser's own counter
(`&self.ext_next_id`) where it used to keep a local `usize`. Sub-pipelines that
consume their fragments internally (`fragment_from_tokens_with_context`, a link
caption's `build_inline_fragment`) keep a local `Cell::new(0)`; none of their ids
reaches the top-level map.

After the fix the minimal probe is:

```
{{Hatnote|Test [[Main Page]].}}
rustoid: <div role="note" class="hatnote" about="#mwt1">Test <a href="./Main_Page">Main Page</a></div>
```

### The second bug underneath: a module's `extensionTag` answer was dropped

With the collision fixed, the stylesheet was gone rather than misplaced. The
`frame:extensionTag('templatestyles', …)` answer is the extension's *output*, which
has no wikitext form in rustoid: it is an `mw:DOMFragment` placeholder.
`lua_deferred::render_answer` converts tokens to their source, and a placeholder
has none, so the answer came back **empty** — every stylesheet a module emitted
this way was lost, which is also why
`templatestyles_test::the_lua_frame_method_reaches_the_same_handler` had been
failing since before `aac41df`.

Scribunto's answer is a `\x7fUNIQ--name-hash-QINU\x7f` strip marker, and modules
depend on that shape: `Module:Infobox` reorders its stylesheets by matching
`\127…UNIQ--templatestyles-…QINU…\127` against `</tr>`, and `Module:Message box`
hands the answer to `mw.html:wikitext()`. rustoid now emits that marker and
substitutes it back:

- `Parser::render_answer_markers` renders a frame call's expansion, replacing each
  `mw:DOMFragment` placeholder with a marker and remembering the placeholder's
  tokens under it (a `<style>` placeholder names its marker `templatestyles`, which
  is what `Module:Infobox` matches).
- `Parser::substitute_strip_markers`, run at the top of `expand_templates`, splices
  the tokens back wherever the module left the marker. The tokenizer splits a
  marker at every `-`, so the runs that hold a sentinel are joined first; a run
  without one keeps its boundaries.

The `about` id is still taken during expansion (that is what
`Module:Infobox`'s numbering needs), and the marker carries the *fragment id*, not
an about id, so re-expansion is not even needed: the marker goes straight back to
the placeholder.

`Template:Infobox`'s realignment (moving its stylesheets before `</tr>`) now has
the marker to work on; whether it lands byte-for-byte is not yet measured.

### Scoreboard after the two fixes

`List of sovereign states`'s first difference moved from byte **477** to **1498**,
and the subset's rustoid total grew from 3 761 177 to 3 879 985 bytes (0.70x →
0.72x) as the missing stylesheets came back. `Help:Introduction` is still at
403.

### What byte 1498 is

The head span and the div now match the service exactly. Two differences remain,
both inside the div:

1. **The div has no `id`.** The service serves
   `<div role="note" class="hatnote navigation-not-searchable" about="#mwt2" id="mwBg">`;
   rustoid omits `id="mwBg"`. `assign_node_ids` keys on a source range or
   `data-mw`, and the div — the range member the wrapper step did *not* move
   metadata onto — has neither (`dp=None dmw=false`). The service gives ids to
   nodes the same walk finds nothing to key: a `<style about="#mwt3" dmw=…>` in the
   same tree also has no id, while `#mwt2`'s div does, so "has an `about`" is not
   the rule either. Not yet diagnosed.
2. **A spurious category link.** rustoid emits
   `<link rel="mw:PageProp/Category" href="./Category:Articles_with_hatnote_templates_targeting_a_nonexistent_page"/>`
   after each link; the service emits none. `Module:Format link` decides the target
   "does not exist", so its `mw.title` existence check is wrong in rustoid — a
   different bug from the ids, and the next thing to look at.

### The div's id: where the search got to

The div *does* have a full `dp` (dsr `[36,457,59,6]`, `stx:html`) and a blob with
that `dsr` at encapsulation time. It loses both to
`cleanup::discard_node`, which runs just before `assign_node_ids` and, for an
interior member of a transclusion range, sets `node.data_parsoid = None`. The id
pass keys on the blob, so the node is left unkeyed.

That mirrors `CleanUp::markDiscardableDataParsoid`, but not its *effect* in PHP:
there the `DataParsoid` object stays in the bag with a `DISCARDABLE_DP` temp flag,
and `storeInPageBundle` still sees NodeData. Two experiments, both wrong:

- Setting `empty_dp_slot` in `discard_node` ("the slot survives, the blob does
  not") gave an id to the first `Category:Articles_with_short_description` link,
  which the service leaves unkeyed — the first difference went *backwards*, to
  byte 348. So a discarded dp does **not** key a node; the service's own
  `$discardDataParsoid` branch suppresses `pbData->parsoid` outright.
- Setting it for every non-transparent range member added ids across the whole
  page (+17 KB) with no improvement.

So the div and the first category link are both discarded interior range members,
yet the service keys one and not the other. `markDiscardableDataParsoid`'s
conditions do not distinguish them (both have `stx`, neither is the range's first
or last node). Reverted; the state is back at byte 1498.

### The right-version sources (and what they settle)

The dead end above was partly a **version trap**: the `/tmp` PHP files were from
different commits. The served page says
`data-mw-parsoid-version="0.24.0.0-alpha23"`, which is the git tag
`v0.24.0-a23`, and the four files that matter are:

| file | why |
| --- | --- |
| `src/Utils/DOMDataUtils.php` | `storeRichAttributes` / `storeInPageBundle` |
| `src/Wt2Html/DOM/Handlers/CleanUp.php` | `markDiscardableDataParsoid` |
| `src/Config/Env.php` | `setupTopLevelDoc` — the `serializeNewEmptyDp` flag |
| `src/Parsoid.php` | `wikitext2html` — `storeInPageBundle` |

What they settle, in the order the value flows:

1. **`storeInPageBundle` is what assigns an id**, and it runs only when
   `wikitext2html` is called with `pageBundle => true`
   (`Parsoid::wikitext2html` passes `$env->pageBundle`). The inline attributes in
   the served HTML are written afterwards, when the page bundle is converted back
   with `toInlineAttributeHtml`. So the *id pass* sees a page bundle, not inline
   attributes, even though the bytes on the wire are inline.
2. **`serializeNewEmptyDp` is `true` for a wt2html top-level document
   (`Env::setupTopLevelDoc`), and it is the load-bearing flag.** With it set,
   `storeRichAttributes` gives **every** element that is not explicitly discarded
   an empty `DataParsoid` — and an empty `DataParsoid` still produces a
   `$pbData`, so the element takes an id while serializing no `data-parsoid`
   (an empty one is not written). That is exactly the "takes an id, emits
   nothing" slot `empty_dp_slot` models, and it is why most elements on a page
   have ids that nothing else explains.
3. **What removes the slot is `CleanUp::markDiscardableDataParsoid`**, which sets
   a `DISCARDABLE_DP` temp flag on interior template-range nodes; a flagged node
   fails the `!$discardDataParsoid` gate and gets no `$pbData->parsoid` at all.
   So "discarded" really does mean "no id" — which is what the reverted
   experiment assumed, and why it looked right for the category link.

**The contradiction that remains.** Under (2) + (3) the div should be discarded:
it is an interior member of the `#mwt2` range, it has `stx` (`"html"`), and it is
neither the range's first node nor its last, so
`( empty( $dp->stx ) || !( last === $node || isHeading ) )` holds either way —
discarded by both branches. Yet the service keys it (`id="mwBg"`). Something in
the traversal's `tplInfo` bookkeeping must make `$state->tplInfo` null (or the
node the range's `last`) for exactly this node, and that is in
`src/Utils/DOMTraverser.php` / `TemplateInfo.php`, not in the four files above.
That is the next file to read — not another guess at the rule.

Also worth fixing on the rustoid side regardless: `cleanup::discard_node` clears
`node.data_parsoid` (the blob) but leaves `node.dp` (the structured object) alive,
so rustoid's two representations disagree about whether a node has metadata. PHP
has one representation and a flag.

### Solved: the traverser's `about`-sibling walk (and why the rule then fails)

`DOMTraverser::traverseInternal` sets `tplInfo` on the first encapsulation
wrapper it meets and — this is the part the four previous files could not show —
**clears it again in two places**:

```php
// after running the handlers on a node
if ( $this->traverseWithTplInfo && ( $state->tplInfo->clear ?? false ) ) {
    $state->tplInfo = null;
}
...
// after advancing
if ( $this->traverseWithTplInfo && ( ( $state->tplInfo->last ?? null ) === $workNode ) ) {
    $state->tplInfo = null;
}
```

So `tplInfo` is live for the range's members **and** for everything *between*
them, and goes null exactly when the walk reaches `last`. What is `last`?
`WTUtils::getAboutSiblings` stops at the first following element whose `about`
differs, then **trims trailing IEW** off the end. `markDiscardableDataParsoid`
reads `tplInfo->first`/`tplInfo->last`, and `DOMUtils::isHeading` is `/^h[1-6]$/`
(`v0.24.0-a23`), so the two boundary exemptions are the range's first node, its
last node, and a heading.

Measured against the real tree, `#mwt2`'s sibling list is:

```
<p> about=None
<span> about=#mwt2 typeof=mw:Transclusion   <- tplInfo->first
<div>  about=#mwt2                            <- interior, stx=html
<span> about=#mwt2                            <- tplInfo->last
```

The div is **interior** (not first, not last), carries `stx: "html"`, and is not
a heading — so `markDiscardableDataParsoid` discards it, and by the rule the
div should have **no** id. The service gives it `id="mwBg"`.

That contradiction is the useful result, because it rules the rule out as the
complete explanation. What it means mechanically: with
`serializeNewEmptyDp` true, discarding a node's `data-parsoid` does *not* remove
the node from the id pass, because `storeRichAttributes` re-creates an empty
`DataParsoid` for it (the `$dp === null` → `new DataParsoid` branch) on the way
into the page bundle — while still suppressing the serialized attribute. Both
facts are measured: rustoid's own div has `dp=Some(…dsr [36,457], stx=html)` at
encapsulation time, and the service emits no `data-parsoid` for it.

So the model rustoid needs is: **a node takes an id when it is an element that is
not a stashed rendering-transparent node** — which is what the `empty_dp_slot`
flag already means, and which the `markDiscardableDataParsoid` port gets wrong by
dropping the slot along with the blob. Setting the slot in `discard_node` was
tried in the previous session and made the *first* category link take an id the
service does not give it, so "not rendering-transparent" is not the right filter
for that node either; the two nodes differ in something the four files still do
not show. Recorded, not guessed at.

## The spurious hatnote category: a fetch is not a fact

`List of sovereign states` served by rustoid carried **two**
`<link rel="mw:PageProp/Category"
href="./Category:Articles_with_hatnote_templates_targeting_a_nonexistent_page"/>`
links that the service does not emit at all. They are not an id or a marking
problem; they are the *module* reaching a branch the reference never reaches.

### The chain, measured

The name reaches `mw.title.new` correctly. Instrumenting `title_derived_field`
(`RUSTOID_TITLE_DEBUG`) shows the exact reads on the real page:

```
TITLEDBG exists name="Lists of sovereign states and dependent territories" answer=false
TITLEDBG exists name="Dependent territory"           answer=false
TITLEDBG exists name="List of countries"            answer=false
TITLEDBG exists name="List of nations"              answer=false
TITLEDBG exists name="Portal:Countries"             answer=false
```

So the names are right and the answer is wrong, and `known=false` for all of them
— which is the actual bug:

```
TITLEDBG miss key="List of sovereign states" text="List of sovereign states"
have=["List of sovereign states/doc", "…/sandbox", "…/testcases",
      "Template:Pagetype/doc", …]
```

The facts map holds only the `/doc`-family subpages. `preload_titles` discovers
titles two ways — literals in module source (`referenced_titles`) and the
`DOC_SUBPAGES` of every frame title — and **a page name arriving as a parameter is
neither**. `Module:Hatnote list` hands `Module:Format link` a name straight out of
`frame.args`, so nothing preloaded it, `title_facts_for` finds nothing, and
`exists` answers `false`. `Module:Format link` then takes its
`categorizeMissing` branch and emits the tracking category.

That is the whole first bug. The second is that the obvious fix for it is wrong.

### Why `exists` cannot simply record the title

`title_derived_field` already has a `note_missing_title` hook, but it is narrowed
to the **Module** namespace, and the comment says why: recording every probe once
took the corpus failure count from 8 to 19. Widening it was tried, because the
narrowing is what leaves `List of sovereign states` broken:

1. **Recording every content title works for the hatnote and breaks
   `Module:Portal`.** With the record widened, the two spurious category links
   disappear and the rustoid total drops 984 718 -> 983 472 bytes — but byte 3
   becomes the first difference, because a new script error appears:

   ```
   module Module:Portal/images/c does not exist ([string "Module:Portal"])
   ```

   Reproduced in isolation with `{{Portal|Countries}}`.

2. **Adding the module preload for the answered title does not help.**
   `Module:Portal`'s `exists()` helper is a *gate* around
   `mw.loadData('Module:Portal/images/' .. subpage .. sandbox)` for all 27
   letters. Answering `exists` true lets the module past the gate, and the
   `loadData` then runs before anything fetched those submodules. Running
   `preload` on the answered title does not fix it, because the submodule name is
   built at runtime (`'Module:Portal/images/' .. subpage`) and so is not a
   literal the scan can see — the same blind spot, one level down.

3. **A third error follows the second** once the gate opens:
   `Module:Redirect hatnote`:94 `bad argument #1 to 'find' (string expected, got
   nil)` — the module read `exists` true, took its `not isRedirect` branch, and
   called `redirTitle:getContent()`, which is `nil` because existence came from
   `get_page_info` while the body was never fetched. Fetching the body too does
   not help, because the body fetch re-enters the same gate problem.

The general shape is the lesson, and it is worth stating plainly: **in this
design `exists` and `loadData` are not independent answers.** A module uses the
first to decide whether to attempt the second, so making existence answerable
without the corresponding fetch does not fill a gap — it moves the failure from a
wrong `false` to a wrong `true`, and the corpus got *worse*, not better.

Everything was reverted; the state is back at byte 1498 and the guard at 876/896.
The correct fix has to keep `exists` and the registry in step, which means the
retry loop needs to fetch the titles a module's *new branch* will go on to
`loadData` — and finding those requires resolving a runtime-built name, which is
the original blind spot rather than a way around it.

## The real blocker was the *name*, and the fetch/registry coupling was a decoy

The previous section ended on a wrong conclusion: that making `exists` truthful
would need the loop to resolve a runtime-built `loadData` target, which is "the
original blind spot". It does not. Running the widened `note_missing_title`
against the real page and *reading the trace* showed the loop was asking for the
wrong title:

```
req Page Countries          # the MissingTitle handler's fetch
```

`Portal:Countries` had been reduced to `Countries` on the way to the data source.
Every name-resolution question above was moot until that was fixed.

### `namespace_prefix` stopped at the core namespaces

`Title::full_text` is the key a `DataSource` is asked about, and it resolves the
namespace through `namespace_prefix`, a hand-written table that ended at
`Module` (828). There was no entry for **Portal (100)** — nor Book, Draft,
TimedText, Gadget, Education program — and a missing entry does not fail loudly.
It returns `""`, `full_text` drops the prefix, and `Portal:Countries` becomes the
key `Countries`: the fetch goes to a main-namespace title, misses, and the miss is
stored as "no such page", which makes the wrong answer permanent for the rest of
the render.

The ids for the namespaces MediaWiki and its shipped extensions register are
fixed by id and cannot vary by wiki, so the table is extended with them. A
wiki-private namespace genuinely has no canonical English name, and the empty
fallback stays correct for it. (The locally-`git`-visible tell was cheap: a
`RUSTOID_TITLE_DEBUG` print in `luafn_title_new` showed `ns_id=100
ns_text="Portal"` — the Lua side was right, and the fault was one call later, in
`full_text`.)

### Widening the record to every namespace is correct

With the name fixed, `note_missing_title` is no longer narrowed to the Module
namespace. Scribunto answers `exists` from the wiki's database and the namespace
plays no part, so the narrowing was the wrong axis; it is what left
`Module:Format link` — reading `mw.title.new(parsed.page).exists` for a page name
that arrived through `frame.args` — on its `categorizeMissing` branch.

The coupling that made the narrowing look load-bearing (a module probing a title
only to decide whether to `loadData` it) is already handled where it happens: a
runtime `loadData` of an unfetched module raises the recoverable missing-module
signal, so existence and the registry do not need to be kept in step by hand.
`Module:Portal`'s `exists` gate therefore opens only when `Portal:Countries` is
actually resolvable — which is a *cache* question, not a code one.

**The lesson from the two reverted attempts stands**, though: an offline run can
only answer for titles it has. Widening the record makes the answer truthful
*where a body is cached*; for a probed title that is not, the answer stays the
conservative `false` and the corpus does not move. This is why the first
difference on `List of sovereign states` did not shift until the four probed
titles were fetched (a `render … online` of the redirect2 probe, ~97 KB: the two
redirects plus two articles). The hatnote category then disappeared and the first
difference moved from byte 1498 to 1945.

## Protection is a fact the wikitext cannot supply

Byte 1945 is the protection block. The page's `{{protection padlock}}`,
`{{pp-move}}` and (on `Help:Introduction`) `{{pp-semi-indef}}` are rendered by
`Module:Protection banner`, which decides between the real padlock and the
`Category:Wikipedia_pages_with_incorrect_protection_templates` tracking category
by reading `mw.title.getCurrentTitle().protectionLevels`. rustoid gave it no
levels, so it always chose "incorrect".

Three separate faults were behind that, and each is general:

1. **The fact was never fetched offline.** `get_title_protection` returned empty
   offline by design, and the online result was not cached. Now cached per title
   under a `prot:` entry kind, so one online run leaves a cache that answers
   offline; `title_protection` also keys its answer by the title *as requested*
   rather than the wiki's spelling, since the caller looks it up by the string it
   asked with.
2. **The page's own protection never reached Lua.** `build_ast` fetches it once
   and stores it in `page_protection`, which serves the `{{PROTECTIONLEVEL:…}}`
   magic word — but Lua reads the same fact through the *title-facts* map, a
   different route. Both are now seeded from the one fetch.
3. **The frame's facts map was being replaced, not merged.** `invoke` seeded the
   frame with the render's accumulated facts, then assigned `frame.titles` the
   return of `preload_titles` — which deliberately contains only titles *not*
   already known. So the seed was discarded and the second `#invoke` on a page
   could not see what the first had resolved. Fixed by merging, which the sibling
   call site already did.

With the fact correct, `Module:Protection banner` now emits the indicator instead
of the bogus category — and the next difference is the `<indicator>` extension
itself: rustoid drops it (`ElementKind::Indicator` exists and maps to `meta`, but
nothing constructs it). Both nearest pages now differ at, or just after, the
protection block:

```
Help:Introduction         first difference at byte 416  (was 403)
List of sovereign states  first difference at byte 1947 (was 1498, then 1945)
fixture guard             876/896
```

Note the wider `<p>` vs `<span>` question in the same block, which is **not** the
protection fact and is still open: parsoid wraps a standalone protected-page
transclusion in `<p class="mw-empty-elt">` where rustoid uses a bare
`<span class="mw-empty-elt">`, while `{{pp-move}}` gets a `<span>` from both. The
two differ in the indicator's presence, so implementing the extension is the
prerequisite for telling the two apart rather than guessing at the rule.

### The `<indicator>` oracle, measured

`Module:Protected page`'s padlock is built from exactly two deferred calls:

```lua
return frame:extensionTag{name = 'nowiki'} .. frame:extensionTag{
    name = 'indicator',
    args = {name = self._indicatorName},
    content = self:renderImage(),   -- [[File:Semi-protection-shackle.svg|20px|…]]
}
```

so `{{#tag:indicator|[[File:…]]|name=pp-default}}` is the minimal input. Asking the
transform endpoint for it (Parsoid `alpha24`, one request) settles the shape
rather than guessing it:

```html
<meta typeof="mw:Extension/indicator mw:Transclusion" about="#mwt1" id="mwAg"
      data-parsoid='{"pi":[[{"k":"1"},{"k":"2"},{"k":"3"}]],"dsr":[0,114,null,null]}'
      data-mw='{"name":"indicator","attrs":{"name":"pp-default"},
                "body":{"extsrc":"[[File:Semi-protection-shackle.svg|20px|…]]"},
                "parts":[{"template":{"target":{"wt":"#tag:indicator","function":"tag"},…}}]}'/>
```

Three things follow, and they are why this is not a one-liner:

1. The element is a **`<meta>`** with `typeof="mw:Extension/indicator"`, and the
   `data-mw` carries the extension's *own* shape (`name`/`attrs`/`body.extsrc`)
   **before** the transclusion's `parts`. rustoid currently emits
   `<extension typeof="mw:Extension" name="indicator" source='…'>` — the raw
   token leaking through, because nothing in `extension_handler::expand_extension`
   matches `indicator`. The body is `extsrc` (unexpanded), which is what
   `templatestyles` already models.
2. It is **not p-wrapped**, unlike the padlock on `List of sovereign states`,
   where the served meta is a *sibling* of a `mw:Nowiki mw:Transclusion` span that
   carries the `about`. So the `#tag:nowiki` answer is load-bearing too: it is
   what makes the transclusion span exist, and the meta then shares its `about`.
   Which of the two shapes appears depends on the enclosing context, so both must
   be reproduced, not just the meta.
3. `data-parsoid` is present in the transform rendering only because the transform
   endpoint keeps it (see the note near the top of this file); the served bytes
   drop it. The transform output is for *shape*, not for bytes.

So the next step is a dedicated `indicator` handler in `extension_handler`, plus
the nowiki interaction above it, not a generic "unknown extension" fallback: most
unimplemented extensions have visible output that a `<meta>` would lose, and only
`indicator` is a meta precisely because MediaWiki hoists it to `<head>`.

## The indicator, and what the placeholder was throwing away

The prediction in the previous section held: implementing `<indicator>` moved both
nearest pages past the protection block. `List of sovereign states` now differs at
the `{{#ifexist:…}}` leak in `Template:Use British English` instead of at the
padlock, and its protection paragraph is **byte-identical** to the served bytes:

```html
<p class="mw-empty-elt" id="mwBw"><span typeof="mw:Nowiki mw:Transclusion"
  about="#mwt4" data-mw='…' id="mwCA"></span><meta
  typeof="mw:Extension/indicator" about="#mwt4" data-mw='{"name":"indicator",
  "attrs":{"name":"pp-default"},"body":{"extsrc":"[[File:Semi-protection-…]]"}}'
  id="mwCQ"/><link rel="mw:PageProp/Category" href="./Category:Wikipedia_…"
  about="#mwt4" id="mwCg"/></p>
```

Two things are worth keeping from this one, because both were surprises that the
served bytes settled and reasoning alone would have got wrong.

### The indicator is not a `ParserHook` element

The `<meta>` is not just *spelled* differently from a normal extension's output —
it is a different construction. A `divtag`/`spantag`/`pre` is a real element whose
body is tunnelled through it; an indicator has no body in the document at all.
Parsoid records the declaration in `data-mw` and MediaWiki renders it into the
page header from there. That is why:

- the body is `extsrc` — raw and **unexpanded**, so the file spec is recorded as
  the module wrote it rather than resolved by us. On the page that is the full
  `[[File:Semi-protection-shackle.svg|20px|link=…|alt=…|This article is
  semi-protected.]]`.

One trap here, recorded because it cost a wrong turn: the *isolated* probe of
`{{#tag:indicator|…|name=pp-default}}` recorded an empty pipe parameter
(`…|20px|`). That is an artifact of the probe, not the rule — in a plain page the
`{{#tag:}}` argument splitter eats the parameter after the pipe. **The page is the
oracle**; the probe is for shape. The rule is the simple one: passthrough.

### The tree builder was discarding the placeholder's metadata

This is the part that cost the time, and it is a general defect rather than an
indicator quirk. A `mw:dom-fragment-token` carried `typeof` and `data-mw` on the
token, and `process_selfclosing` threw both away: it built the placeholder
attributes from scratch as

```rust
let attrs = Attributes::from_pairs(vec![
    ("typeof".to_string(), "mw:DOMFragment".to_string()),
    (DATA_OBJECT_ATTR_NAME.to_string(), id.to_string()),
]);
```

Every placeholder therefore became a typeless `<meta>`, and the *fragment* it
stood for — a bare `<meta>` with no attributes of its own — could not supply the
missing type at unpack time. Any future extension shaped like this (metadata
recorded on the extension, element carrying no body) would have failed the same
way, which is why the fix is in `stash_fragment`/`process_selfclosing` rather
than in the indicator handler.

`mw:DOMFragment` stays the fallback when the token carries no `typeof`, so
transclusion markers are untouched; the guard is on absence, not on a whitelist.

### Scoreboard

```
List of sovereign states  first difference at byte 1947 (protection block now exact)
Help:Introduction          first difference at byte 416
fixture guard              876/896
clippy                     0 warnings
```

Both are now blocked on the *same* class, which is the next thing to take: an
`{{#ifexist:Category:{{{1}}} {{{2}}} {{{3}}}}}` reaching the output unexpanded
through `Template:Use British English` → `Module:Unsubst`. It is the
lazily-expanded-argument-value family already recorded for `Bicycle`.

## The lazily-expanded argument value, and the two wrong answers

`Bicycle`'s remaining difference was the short-description category:
`Short_description_is_different_from_Wikidata` where the service emits
`..._matches_Wikidata`. `Module:SDcat` compares the local short description with
`mw.wikibase.getDescription()`, and the local one arrives through
`frame:getParent().args.sd`; the module was reading `{{{1|}}}`.

The chain is `Short description → Main other → SDcat`, and the value's *tokens*
were correct the whole time. What was wrong was which representation the parser
read back:

- `kv.value` — the expanded tokens — held the description.
- `kv.src_offsets` — the recorded source range — still held the wikitext as
  written, `{{{1|}}}` included.

`frame_args_to_lua` preferred the range (`kv_value_source`), so the module got the
stale source. The fix reads the tokens **when they are all plain text**, which is
exactly when the range is stale and the tokens are complete.

### Why not simply prefer the tokens

Because rustoid's token→text rendering is lossy, and preferring the tokens
everywhere traded one wrong rendering for another. Measured, not assumed: with
`frame_args_to_lua` reading the tokens unconditionally, `Megadeth`'s hatnote —
`For a definition of that term, see the Wiktionary entry megadeath` — collapsed to
`, see the Wiktionary entry ` and the page lost 50 KB, and the same shape cost
`Sundial` 70 KB. `tokensToString` has no arm for a wikilink, a tag, or an
unexpanded parser-function token, and drops each; a value made of those has no
faithful textual form in rustoid today, so the range remains the lesser evil for
it. **Rendering those tokens faithfully is the follow-up**, and it is what would
let the range be dropped entirely.

A second attempt — invalidating `src_offsets` at the substitution itself, in
`Frame::expand`'s attribute walk — is the *right place* conceptually and the
wrong one in practice: the same `KV` is read by the DOM pipeline, so dropping the
offsets there also changes `data-mw`/attribute reconstruction and moved content
far beyond the Lua path (Bicycle lost 148 KB). The offsets need to be split, or
the Lua-facing text computed at expansion time and stored, before that can work.
Both attempts were reverted; the record above is what they measured.

### The first argument was never expanded at all

Found while tracing the above, and a bug in its own right: `expand_invoke_args`
iterated `args.iter_mut().skip(1)` with the comment "`args[0]` is the `#invoke:`
target and has already been resolved". That is true of the call's own argument
list and false of a **parent frame's**, which has no target — so the calling
template's *first* argument was left unexpanded. It is the position
`{{see Wiktionary|…}}` uses to hand its text to `Module:Hatnote`, which is why the
hatnote reads `args[1]`.

The fix passes the arguments to expand as a slice, so each caller says which list
it means (`&params.args[1..]` for the call's own, the whole list for the parent's)
and the two cannot be confused again.

### Scoreboard

```
Bicycle                   first difference at byte 733 (was 538); category agrees
List of sovereign states  1947   (the {{#ifexist:…}} leak below, still open)
Megadeth                  1650   (unchanged)
fixture guard             876/896
subset total              3 880 069 bytes (was 3 880 042)
```

The `List of sovereign states` difference is still the `{{#ifexist:Category:{{{1}}}
{{{2}}} {{{3}}}}}` reaching the output through `Template:Use British English` →
`Module:Unsubst`. That is a different shape from the above — a `{{safesubst:}}`
invocation whose `$B` body is returned by the module and then re-expanded — and it
is *not* fixed by any of this, so it is the next thing to reduce.

## The faithful argument renderer: understood, built, and not landed

The follow-up promised in the previous section was to render a module's argument
value from its tokens *completely*, so the stale source range could be dropped
rather than worked around. It was written, measured, and reverted — the
measurements are the result.

### What the tokens actually are

Instrumenting `expanded_argument_text` on `Megadeth` gives the shapes a module's
arguments are made of. For the hatnote value
(`{{see Wiktionary|…}}` → `Module:Hatnote`'s `args[1]`) the `#if`s are expanded
(it is the *first* parent argument, which the previous section's fix stopped
skipping) and what remains is:

```
[" ", "\"", "Megadeath", "\" redirects here. For a definition of that ",
 "term", ", see the Wiktionary entry ",
 sc:wikilink src="[[wikt:{{{2}}}|{{{2}}}]]" attrs=[href=wikt:megadeath,
                                              mw:maybeContent="megadeath"],
 "."]
```

Two things follow. The wikilink's **attributes are substituted** while its
`dp.src` is not, so the source cannot be used and the attributes must be — which
is exactly what `wikilink_source` (now shared by `reconstruct_link_src`) does.
And the value is otherwise plain text, so a renderer that handles the wikilink
makes it faithful.

The navbox-shaped values on the same page are different: they interleave
`sc:mw-quote` markers (`''`/`'''`), `<div>`/`<ul>`/`<li>`/`</style>` tags, and
`sc:extension`. `tokensToString` has no arm for `mw-quote` or for a tag, so it
drops them: `''[[Foo]]''` stringifies to `[[Foo]]`.

### Why it was not landed

- **Adding the display text to the shared `tokensToString` arm is wrong.** That
  renderer also feeds attribute values on the DOM path: with it changed, a
  `Module:Navbox` title attribute's length changed, and `Sundial` rendered
  `[[Philosophy of space and time</th>` — an *unrendered* link — and lost 13 KB.
  The argument renderer must be separate, which is what
  `argument_tokens_to_string` was.
- **Separate and contained, it has no measurable effect on the scoreboard.** With
  the renderer used only for values whose tokens contain nothing it would drop,
  the subset is byte-for-byte unchanged and no first difference moves: the real
  values carry `mw-quote`/tag tokens, so the containment excludes precisely the
  values that would benefit. Widening it to render those made `Megadeth`'s
  hatnote value faithful and moved that page's first difference 1650 → 1595 — but
  it also shifted ~10 KB of already-diverged content on `Sundial`, `Megadeth` and
  `Nobel Prize` in ways that were not shown to be improvements, and one shape
  leaked a literal `[[Template:Ndash]]` into a Cite id.

So the prerequisite is a renderer that covers *every* token an argument value can
hold — `mw-quote` and tags included, tags from their start/end `dp.src` — before
the source range can be dropped, and the containment predicate **is** the
statement "the renderer is complete", which is why it cannot be relaxed
piecemeal. That is a bigger job than this session had room for, and landing it
half-done would trade a mis-resolution for a silent loss.

Reverted; the tree is back at the previous section's state.

## The `Use British English` leak is `#ifexist`, and nothing else

`List of sovereign states`'s difference at byte 1947 was recorded as a
`{{#ifexist:Category:{{{1}}} {{{2}}} {{{3}}}}}` reaching the output through
`Template:Use British English` → `Module:Unsubst`. Reducing it settles what the
Unsubst/`safesubst:` part actually costs: **nothing**. The safesubst invocation is
handled — `$B` is returned by the module and re-expanded, `{{{date|}}}` is
substituted in the frame that wrote it — and the leak is reachable without any of
it:

```
{{Dated maintenance category|1=A|2=B|3=C|4=D}}
```

on a plain page produces

```html
<link rel="mw:PagePropCategory" href="./Category:A_B_C"/>
<span>{{#ifexist:Category:{{{1}}} {{{2}}} {{{3}}}
    | … |[[Category:Articles with invalid date parameter in template]]}}</span>
```

Note the two failures in one fragment. `[[Category:A B C]]` — the branch
*condition* — is substituted and rendered correctly, so the frame work in the
path is fine. The `#ifexist` is emitted **verbatim from the token's source**, with
its arguments still carrying `{{{1}}}`.

### Why

`{{#ifexist:…}}` is not implemented. It falls through to the unknown-parser
function arm of `TemplateHandler::call_parser_function`, whose contract is
"preserve the original source verbatim" — `token_src`, the wikitext the token was
tokenized from. That is the right answer in *standalone* Parsoid (which has no
wiki to ask and emits `Parser function implementation for pf_ifexist missing`),
and the wrong one in integrated mode, where the wiki answers it.

The `{{{1}}}` is not a separate bug: the source is what the token was tokenized
from, so it holds the body's text before substitution. Re-rendering the fallback
from its (substituted) attribs is therefore free and more faithful — but it is not
a parity gain, because the `#ifexist` itself still would not run, and the branch it
selects is the difference.

### What implementing it costs

`#ifexist` is a *synchronous* parser function whose answer needs an async
existence check — the same shape as `PROTECTIONLEVEL`, which rustoid answers from a
pre-pass (`collect_protection_titles`) that fetches `get_title_protection` before
expansion runs. That pattern **cannot** be reused here, and the reason is worth
recording because it is the general obstacle:

- The title to check is written inside a template's body
  (`Category:{{{1}}} {{{2}}} {{{3}}}`), and at pre-scan time — before expansion —
  its arguments are unsubstituted, so the string a pre-pass would collect is not
  the title. `collect_protection_titles` has the same hole, and the notebook
  already records that a `{{PROTECTIONLEVEL:…}}` arriving from a template "reads as
  unprotected"; for `#ifexist` that would mean taking the *wrong branch* on every
  such call, which is worse than the current visible leak.
- The existence map the parser already builds (`add_red_links`) is a *DOM
  post-pass*, so it is not available during expansion either.

So a faithful `#ifexist` needs a **deferred answer for magic words**: raise a
request while expanding, have the host fetch the title, and re-run the expansion
with the answer — which is the mechanism `frame:expandTemplate` and friends
already use for Lua (`pipeline/lua_deferred.rs`), generalized from module frame
calls to parser functions. That is the next architectural step, not a local fix,
and it also carries MediaWiki's 500-call expensive-parser-function limit.

Recorded rather than attempted: the session had no room for a mechanism of that
size, and a half-answer (branching on an unsubstituted title) would be a
regression dressed as progress.

## Two contained bugs, and the page a node-count trip had been eating

The next session went looking for a difference to reduce and found two, both in
the *template metadata* path rather than in expansion. Neither is architectural,
and the second one is the largest single win recorded here so far.

### An empty template argument is `"wt":""`, not `"wt":null`

`TemplateInfo::toJsonArray` writes `'wt' => $info->valueWt` unconditionally, and
every builder (`prepareTplParamInfos`, `preparePfParamInfos`,
`prepareTemplate3ParamInfos`) assigns `valueWt` a real source string — possibly
empty, never null. rustoid's `OrderedJson::put_opt_str` collapsed an empty value
to JSON `null` instead, so `{{About||the butterfly genus|Bicyclus…}}` recorded
`{"1":{"wt":null}}` where the service serves `{"1":{"wt":""}}`.

A one-line change (`put_str` instead of `put_opt_str`, and `put_opt_str` was then
dead and removed). It moves `Bicycle`'s first difference 733 → 1588. Neither
spelling is pinned by a fixture — the suite has no `"wt":""` or `"wt":null` at
all — which is why the wrong one survived.

### A named argument reaching Lua was not trimmed

`Frame::expand_template_arg` trims a *named* argument's value for `{{{name}}}`
substitution, and MediaWiki's preprocessor trims it before Scribunto sees the
call at all — but `frame_args_to_lua` passed the expanded text through
untrimmed. The visible consequence was `Zebra`.

`Zebra`'s taxobox is `{{Automatic taxobox | taxon = Equus (Hippotigris)}}`. The
module got `" Equus (Hippotigris)"` with a leading space, built
`Template:Taxonomy/ Equus (Hippotigris)` *with the space*, missed it, and then
walked a parent chain that never terminated: `Template:Taxonomy/` (empty taxon),
`[[Template:Taxonomy/]]`, `Taxonomy/{{Taxonomy/[[:Template:Taxonomy/]]|machine
code=parent}}`, and onward, each level expanding the last. It reached
**1,000,000 preprocessor nodes** and tripped the node-count limit, after which
*every* later expansion on the page was the error span.

That is why `Zebra` was 113 KB against the service's 560 KB, and why its first
difference sat at byte 444 as a nonsense category
(`Node-count_limit_exceeded_with_short_description`). With named values trimmed,
the page renders 632 KB in 1.9s (was 147 KB in 17s), it no longer trips the
node-count limit at all, and its first difference moves 444 → 1486.

The limit itself was never wrong; a million expansions is a real trip. The bug
was upstream of it, and the trap is worth naming: **a runaway expansion reports
as a limit, not as a miss**, so the diagnosis has to start from *what* is being
expanded (the debug counter over `expand_templates`' target showed the
`Taxonomy/` chain immediately) rather than from the limit value.

### Scoreboard

```
List of sovereign states   first difference at byte 1947   (unchanged)
Help:Introduction          416
Quicksilver (film)         590
Unix                       904   (was 852: protection now cached, see below)
Sundial                    1460
Bicycle                    1588  (was 733)
Nobel Prize                600
Megadeth                   1650
Zebra                      1486  (was 444; page no longer eaten)
fixture guard              876/896
subset total               parsoid 5 351 722 / rustoid 4 290 867 (0.80x, was 0.73x)
```

## What is actually left, and one prerequisite that keeps recurring

The remaining first differences fall into a small number of groups, and **three
of them need the same thing the cache does not yet store**. Recording the shape
because it is the recurring obstacle, not a one-off.

### The offline cache stores content, not facts

`get_page_info` (link existence, `mw-disambig`, `mw-redirect`) is answered by the
harness's `page_info`, which fetches `prop=info`. **Its result is never cached.**
Offline, `get_page_info` therefore returns “everything exists, no link classes”
(`page_info_soft`/`existing()`), so:

- `Module:Format link`'s `title.exists` check is answered from *content*, and an
  uncached-but-real target reads as a red link — `Bicyclus`, `Zebro`,
  `Sundial (disambiguation)` all produced a spurious
  `Articles with hatnote templates targeting a nonexistent page` category
  (`Bicycle` byte 1588, `Zebra` 1486, `Sundial` 1460).
- `mw-disambig` is applied by `add_red_links` from `PageInfo::linkclasses`, which
  `page_info` never fills (it does not request `ppprop=disambiguation` at all).
  Offline it could not be filled even if it did.
- `#ifexist` (see above) would read “every title exists” offline, so the branch it
  selects would be wrong for exactly the maintenance categories that usually do
  not exist — worse than not implementing it.

So the prerequisite for the next several fixes is a **cached page-info fact**:
store `{missing, known, redirect, linkclasses}` per title, populated by an online
run, and read offline. That is the honest fix for the hatnote categories, for
`mw-disambig`, and for `#ifexist`; patching any of them to the *live* wiki's
existence without it would only move the lie.

A cheaper stopgap was tried and *reverted*, and the record matters: making the
Lua `TitleFacts` derive `exists` from `get_page_info` when content is absent.
It is more faithful on the live wiki, but offline it asserts `exists = true`
while `content` stays `None`, and modules read on from there —
`Module:Listen` indexed `title.file` on a file it now “knew” existed,
`Module:Redirect hatnote` called `:find` on a nil `getContent()`. It surfaced
seven new Lua failures. The lesson is the same as above: **`exists` and
`content` must come from one source**, or one of them is a claim the render
cannot honour.

### Cache populated this session

Running the subset **online once** (with the baseline guard in place, so no
pinned revision was touched) filled the protection facts and the disambiguation
pages that the renders ask about. Offline output total went 4 193 380 → 4 290 867
bytes and `Unix`'s first difference 852 → 904 (its `prot:` entry was the
missing fact). The run hit HTTP 429 partway through the last four pages, which
is itself the reason the next population should be targeted rather than a whole
corpus: the requests are mostly cache *hits*, but the misses are per-page and
the API is not sized for nine at speed.

### The `{{!}}` in a hatnote argument was reaching the module as `{{!}}`

Chasing `Bicycle`'s hatnote category past the cache gap showed the title was not
`Bicyclus` at all: the probe's fetch trace said `Page Bicyclus{{!}}''Bicyclus''`.

The wikitext is
`{{About||the butterfly genus|Bicyclus{{!}}''Bicyclus''|other uses}}`. Scribunto
receives each argument as *text*, and the preprocessor's `{{!}}` is a `|`, so the
module's `parseLink` sees `Bicyclus|''Bicyclus''`, splits it, and checks
`Bicyclus`. rustoid instead handed it the literal `Bicyclus{{!}}''Bicyclus''`,
so the page it checked for existence was the whole string, and the blue link
gained a nonexistent-page category.

The cause is the argument renderer's precondition, not the tokenizer. Inside a
template, `process_special_magic_word` produces a `<td>` marker for `{{!}}` (so
`TableFixups` can reclaim a cell separator) — right at the token level, wrong as
text. `expanded_argument_text` only trusted a value whose tokens were *all*
`Str`, so a value carrying the marker fell back to the source range, which still
spells `{{!}}`.

`argument_value_text` now has an **exact** form for precisely the token shapes a
plain-text value is made of — `Str`, `mw-quote` (its `value` attribute holds the
`''`/`'''` delimiter, and in Scribunto's string view `''x''` is literally that),
and the `{{!}}` marker (`|`) — and returns `None` for *any* other token, which
keeps the source range as the fallback. That containment is the whole safety
argument: values with a wikilink, a tag, or any other construct render exactly
as before, so the only behaviour that moves is a value carrying `{{!}}` or
quote markup, both of which the old path got wrong (`tokens_to_string` has no
`mw-quote` arm and drops the quotes).

**Applied to the parent-frame arguments only.** The `#invoke` call's own
arguments are still re-joined as *text* (`invoke_arg_text` joins on `|` before
`Invoke::parse` splits them again), so rendering a `|` there splits one argument
into two: `{{#invoke:String|len|a{{!}}b}}` went from wrongly answering `7` to
wrongly answering `1`. That path needs the arguments handed over *structured*
rather than as a reconstructed string; until then it keeps the old rendering on
purpose, and the reason is written at the call site.

### Scoreboard

With the renderer fixed, `Bicyclus` was the *only* thing left in `Bicycle`'s
hatnote, and a targeted online run (pinned to the cached revision, so it did not
resolve or fetch a new baseline) cached it and `Zebro` alongside. That is what
moves the two pages:

```
Bicycle   1588 -> 1697   (the residual is mw-disambig, below)
Zebra     1486 -> 2114   (the residual is mw-disambig)
Sundial   1460           (mw-disambig only; content was never the problem)
```

So the hatnote *existence* category is now gone from all three; what is left on
them is `class="mw-disambig"` on a link to a disambiguation page, which is
`add_red_links`'s `linkclasses` (see above) and needs the page-info fact.

### The rest

- `Help:Introduction` (416) — an adjacent-transclusion **encapsulation merge**:
  the service groups `{{pp-semi-indef}}` and `{{intro to single}}` (with the
  literal `</noinclude>` between them) into one `#mwt4` wrapper; rustoid emits
  only the second, so the `mw:Transclusion` part list is short and the id is one
  low.
- `List of sovereign states` (1947) — the `#ifexist` leak, **confirmed** rather
  than inferred. The `<p>` the service serves as `class="mw-empty-elt"` holds,
  on rustoid's side, the empty `mw:Nowiki` protection span, the indicator
  `<meta>`, and then the two leaked `{{#ifexist:Category:{{{1}}} {{{2}}} {{{3}}}`
  spans. The leak is *text*, so `is_empty_node` refuses to mark the `<p>` empty
  and the class goes missing at 1947. This closes the loop the previous section
  opened: the leak and the missing class are one difference, not two.
- `Unix` / `Nobel Prize` protection categories — a missing `prot:` fact, now the
  cheap half of the page-info problem and already partly populated.

### The probe was lying about entities

`examples/render` did not attach the Wikidata wiki, so any page touching
`mw.wikibase` diverged from a real comparison run — `Module:SDcat` reported
"empty Wikidata description" for a page whose entity was cached, which sent a
diagnosis after a bug that was not there. It now builds the same `SiblingWiki`
`harness::with_entity_wiki` does. A probe that fails to reproduce the real run is
worse than no probe, because it manufactures confident wrong answers.

## Cached page-info, and `#ifexist`

### The `PageInfo` cache kind

The prerequisite the previous section named is now built. `EntryKind::PageInfo`
(prefix `info:`) stores `{missing, known, redirect, linkclasses}` per title, and
`get_page_info` prefers the cache, fetches only what it does not have, and stores
what it fetches. A transport failure still answers conservatively but is **not**
written, so a guess cannot outlive the failure it came from.

The other half was in `page_info` itself: it never requested `ppprop=disambiguation`,
so `linkclasses` was always empty and `mw-disambig` could not exist even online.
The property comes from the `__DISAMBIG__` magic word and is not in the target's
body, which is the whole reason it is a cached fact.

Populated for the subset, this moves the three hatnote pages:

```
Sundial   1460 -> 1897
Bicycle   1697 -> 2465
Zebra     2114 -> 2172
```

### `#ifexist`, ported

`#ifexist` is now implemented rather than falling through to the
unknown-parser-function arm that leaked its own source. Core's semantics: answer
`then` when the title exists, `else` otherwise, and **do not trim** the branch
(`trimmed_branch` mirrors `#if`; `untrimmed_branch` is the separate rule).

The mechanism is the one the earlier section designed: `#ifexist` is synchronous
but existence is a fetch, so the answer is resolved *at the call site* —
`expand_template_token`, once the target's `{{{…}}}` are substituted — into the
parser's `ifexist` map, which the synchronous `call_parser_function` then reads.
It is **not** a pre-scan, and that distinction is the point: a pre-scan over the
unexpanded stream would collect `Category:{{{1}}} {{{2}}} {{{3}}}` instead of the
title, which is the wrong-branch trap recorded two sections up. The map is
deduped by title and capped at 500 entries, mirroring MediaWiki's budget for
expensive parser functions.

**A caveat, recorded because it is load-bearing.** The port is faithful by
construction, but the *input* is not, in exactly the path that motivated it: the
`Use mdy dates` / `Use British English` stack reaches `#ifexist` through
`Module:Unsubst`'s `$B`, and there the title is still `Category:{{{1}}} {{{2}}}
{{{3}}}` — because the argument that should carry `A`/`B`/`C` is lost upstream
(below). So today the branch it takes in that path is right only by luck (offline
it reads "exists", which happens to match). Fixing the argument expansion makes
`#ifexist` correct for free; until then it is a prerequisite, not a fix. It also
moved no first difference on its own — it removed the leaked source and made the
output slightly smaller — but it is the piece the earlier section said had no
room to land.

### The real blocker: an expanded argument cannot be rendered back to text

The `$B` argument is where the `Use mdy dates` family actually fails, and it is
one minimal reproduction:

```
{{#invoke:Unsubst||$B={{DMCA|A|B|C}}}}
```

renders `[[]]` where the service renders `Category:A_B_C`. `Module:Unsubst`
returns `frame.args['$B']`, and what rustoid hands it is not
`[[Category:A B C]]` but `[[]]`.

The chain: `expand_invoke_args` expands `{{DMCA|A|B|C}}` — and the arguments do
arrive, the trace shows `Template:Dated maintenance category` called with
`1=A;2=B;3=C` — but the *result* it stringifies is a `wikilink` token whose
`href` is not set yet. `href` is assigned by `render_links`, a later pass, so
`tokens_to_string`'s wikilink arm reads an empty `href` and emits `[[` + `` +
`]]`. The faithful text Scribunto receives is the *substituted wikitext*
`[[Category:A B C]]`.

This is the argument renderer again, and it is the same shape the earlier section
could not land: rendering an expanded argument back to its wikitext needs every
token covered, and a wikilink is one `tokens_to_string` gets wrong *when the link
has not been rendered*. The narrow fix (fall back to the token's own source) is
not obviously right either, because the recorded source is the *unsubstituted*
body. It is recorded here rather than attempted with the session's remaining
room, and it is what stands between the `Use …` stack and every page that uses
it.

### Scoreboard

```
Help:Introduction          416
Quicksilver (film)         590  (ifexist leak gone; the [[ ]] below is the difference)
Unix                       904  (same)
Zebra                     2172
Sundial                   1897
Bicycle                   2465
Nobel Prize                600
List of sovereign states  1947
Megadeth                  1650
fixture guard             876/896
subset total               parsoid 5 351 722 / rustoid 4 330 015 (0.81x)
```

## The argument renderer, and expanding a link target

The two jobs the previous section deferred were the argument renderer and the
title-substitution inside a link. Both are now in, and the `$B` family — the
`Use mdy dates` / `Use British English` / `Use American English` stack behind six
of the nine pages — renders its categories.

### The renderer covers a link, a tag and quotes now

The renderer only trusted plain text, `mw-quote` and the `{{!}}` marker, so any
value that expanded to a *link* fell back to its source range. That range is the
wikitext as written, which for a substituted value is stale, and for the `$B`
argument of `Module:Unsubst` is the template call itself. Worse, the `#invoke`
path rendered such a value with `tokens_to_string`, whose wikilink arm reads the
token's `href` as a *string* — but the href is a **token list** (the target is
tokenized so a templated target can expand), so `as_str` returned `None` and the
value arrived as `[[]]`.

`{{#invoke:Unsubst||$B={{DMCA|A|B|C}}}}` is the minimal case, and it now renders
`Category:A_B_C`. `{{Use mdy dates|date=February 2026}}` renders
`Category:Use_mdy_dates_from_February_2026`, where before the whole value was
`[[]]`.

The renderer — `argument_value_text`, deliberately **separate** from the shared
`tokens_to_string` that also feeds DOM attribute values — now covers:

- `Str`; comments and newlines (dropped, as `tokensToString` drops them);
- `mw-quote` (from its `value` attribute) and the `{{!}}` marker (`|`);
- `wikilink`, rebuilt from its `href` and `mw:maybeContent` fields and recursing,
  because those are token lists;
- `extension`, from its source (`<nowiki/>`).

Anything else still returns `None`, so the containment — *a value is rendered from
tokens only when every token has an exact textual form* — is unchanged. The
`#invoke` call is also built **structurally** now (`Invoke::from_parts`) from the
module and the expanded arguments, instead of being re-joined into
`M|f|a|b=c` text and re-parsed: a value may contain `|` (a link's display text)
or `=`, and the text round-trip cannot tell either from a separator.

### A link target is a token list, and expansion has to look inside it

`expand_templates` walks the top level of a chunk and leaves a `wikilink` alone,
because the target is a token list *inside* the token. On a page the
`expand_attributes` pass resolves it afterwards; an argument has no such pass, and
Scribunto receives the expanded text. So `{{#invoke:String|len|[[{{PAGENAME}}]]}}`
answered 16 (the source) where the service answers 8.

The pre-check in `expand_invoke_args` was the second half of the bug: it scanned
only *top-level* items for a `template`, and `[[{{PAGENAME}}]]` has none there, so
the value was skipped entirely. It now looks inside tokens (`expandable_content`),
and the expansion half of `expand_attributes` runs on the value
(`expand_attrib_templates`) — without the `mw:ExpandedAttrs` marking, which
describes the DOM a link becomes rather than the string a module is handed.
`[[{{PAGENAME}}]]`, `[[{{lc:ABC}}]]` and `[[{{#if:1|Yes|No}}]]` all answer the
expanded length now.

### The size metric became honest, and that is not a loss

The subset total fell from 4.33 MB to 4.01 MB (0.81x → 0.75x) across these two
changes. Almost all of it is ~318 KB of *garbage* that the old render emitted: a
runaway `data-mw` in which a `{{col begin}}` transclusion listed the rest of the
page as a literal string part and leaked `{{#ifeq:{{{bigbox` as text. That leak
is now zero (matching the service), the two affected lines are otherwise the only
difference between old and new (verified by line diff — every other line is
byte-length-identical), and the page's tail, categories and tables are intact. So
the ratio was inflated before; it is now the real one.

### Scoreboard

```
Help:Introduction          416
Quicksilver (film)         590 -> 1551
Unix                       904 -> 1736
Zebra                     2172
Sundial                   1897  (media: the file info is not cached)
Bicycle                   2465 -> 3168
Nobel Prize                600
List of sovereign states  1947 -> 2771
Megadeth                  1650
fixture guard             876/896
subset total               parsoid 5 351 722 / rustoid 4 007 778 (0.75x)
```

### What the remaining first differences are

Reduced to four shapes, none of them the `$B` family any more:

- **`about` id off by one** — `Bicycle` (3168) and `List of sovereign states`
  (2771) both show rustoid one transclusion id *behind* the service, so something
  earlier took an id it should not have, or missed one it should. This is the
  cheapest of the four and the next thing to reduce.
- **The encapsulation merge** — `Help:Introduction` (416) and `Zebra` (2172): the
  service groups adjacent transclusions (`{{pp-semi-indef}}`, the literal
  `</noinclude>`, `{{intro to single}}`) into one wrapper with several `parts`;
  rustoid emits them separately. `Zebra` also shows an indicator whose `attrs.name`
  is the single space `" "`.
- **The protection category** — `Nobel Prize` (600): rustoid emits
  `Wikipedia pages with incorrect protection templates` where the service emits
  the real protection. The `prot:` fact is populated for some titles and not this
  one, which is the offline half of the page-info problem and a targeted fetch.
- **Media** — `Sundial` (1897): rustoid renders `mw-broken-media` because the
  file info is not cached, which is a cache gap rather than a parser gap.

## The `about`-id off-by-one: an extension spends an id it never emits

Both pages with a protection template were one id *behind* the service from the
padlock onward, and the shape was narrow enough to name: on `Bicycle` the
emitted ids are `1…6` for the head, then the service has no `#mwt7` at all and
uses `#mwt8` for `{{Use dmy dates}}`, while rustoid used `#mwt7`. On
`List of sovereign states` the same gap sits one block earlier — the service
spends `#mwt5` between `{{pp-semi-indef}}` and `{{pp-move}}` and emits it
nowhere. Both gaps fall immediately after a template whose module emits an
`<indicator>`.

The cause is `ExtensionHandler::onDocumentFragment`. For **every** extension tag
except `nowiki`, Parsoid does:

```php
$about = $env->newAboutId();
$n = $firstNode;
while ( $n ) { $n->setAttribute( 'about', $about ); $n = $n->nextSibling; }
```

`nowiki` is special-cased to lean markup and skips the block; everything else
spends an id at the moment its token is processed, *in document order*. The id
is frequently not the one that survives: an extension emitted by a template is
a child of the transclusion, whose encapsulator overwrites `about` with its own.
The indicator is exactly that case — the served `<meta>` carries the
`pp-protected` template's id — but the spend still happened, so every following
transclusion is numbered one higher than the emitted ids suggest.

### It has to be spent *during* expansion, not in the indicator pass

The first attempt spent the id in `expand_templates`, for any `extension` token
whose name is not `nowiki`. That over-corrected by one: the indicator's token is
walked **twice**. `Module:Protected page` reaches `<indicator>` through
`frame:extensionTag`, which rustoid lowers to `#tag` and answers on a deferred
re-run; that re-run's own `expand_templates` sees the token, and then the
module's output — which still carries the raw marker — is tokenized and walked
again. Two spends, one indicator.

The asymmetry is visible against `<templatestyles>`, which is already correct:
its `#tag` expansion builds the real `<style>` placeholder *inline* in
`expand_templates`, so the callback stores the placeholder under a marker and
the final walk splices the placeholder rather than re-expanding a token. The
indicator was handled only by the separate `expand_indicator` pass, which runs
after all expansion, so the callback could not build its placeholder and the
token survived to be expanded a second time.

The fix mirrors templatestyles: a new `Parser::expand_one_indicator` resolves
the token inline, spends the `about` id, stashes the `<meta>` fragment in
`self.ext_fragments`, and emits the placeholder. Now the callback produces the
placeholder, the final walk splices it once, and the id lands in expansion
order. The leftover `expand_indicator` pass is kept as a safety net for tokens
that never pass through `expand_templates` (it does not spend an id, so those
rare cases would still be one low — recorded rather than papered over).

The rule worth carrying forward: **a document-level `about` id is spent where
the extension's token is processed, even when the id is thrown away.** Any
extension rustoid handles in a pass that runs after expansion will be one id
low unless the pass runs inline, as templatestyles and now the indicator do.

### Scoreboard

```
Help:Introduction          416 -> 492
Bicycle                   3168 -> 3577
List of sovereign states  2771 -> 4009
fixture guard             876/896
subset total              4 007 778 -> 4 006 549
```

`Bicycle`'s new first difference is a `data-mw` target `wt` that keeps the
newline before the first `|` (`"Infobox machine\n"` where rustoid trims it);
`Help:Introduction` is the encapsulation merge; `List of sovereign states` now
reaches its first missing file-info fact at the map figure.

## `data-mw.target.wt` is the target's *source*, not its cleaned name

The `Bicycle` diff above was the visible half of a general bug: rustoid recorded
`info.target_wt` from the cleaned, trimmed target string, so every transclusion
whose target carried trailing whitespace (a newline before the first `|`, a
space before a wrapped call) lost it in `data-mw`.

PHP takes it from the *source range* of the target key
(`TemplateEncapsulator::getTemplateInfo`):

```php
$tgtSrcOffsets = $params[0]->srcOffsets;
if ( $tgtSrcOffsets ) { $ret->targetWt = $tgtSrcOffsets->key->substr( $src ); }
```

so the rule is *leading whitespace skipped, everything else kept*. Confirmed
against the transform oracle, one request, five shapes:

| wikitext | `wt` |
|---|---|
| `{{Template:Infobox machine\n|…}}` | `Template:Infobox machine\n` |
| `{{  Template:Infobox machine  |…}}` | `Template:Infobox machine  ` |
| `{{ Template:Infobox machine|…}}` | `Template:Infobox machine` |
| `{{Template:Infobox mach<!--x-->ine|…}}` | `Template:Infobox mach<!--x-->ine` |
| `{{Template:Infobox machine<!--c-->|…}}` | `Template:Infobox machine<!--c-->` |

Note the fourth row: the comment is *kept in `wt`* even though resolution strips
it (the title resolves to `Template:Infobox_machine`). So the two strings are
genuinely different — resolution uses the cleaned key, `wt` the raw source — and
rustoid now keeps both, deriving `wt` with `raw_template_target(…).trim_start()`
at the call site and passing it into `expand_one_template`.

### Scoreboard

```
Bicycle                   3577 -> 6979   (now the file-info cache)
Quicksilver (film)        1551 -> 2732
fixture guard             876/896
subset total              4 006 549 -> 4 006 648
```

## `#invoke` arguments are expanded lazily, and the id order shows it

`Quicksilver`'s new first difference is a `<style about>` id: the service has
`#mwt5` on `Module:Infobox/styles.css`, rustoid `#mwt14`. Nine ids sit between
the Infobox wrapper (`#mwt4`) and its own stylesheet, and a labelled trace names
them at once — all nine are `Plainlist/styles.css`:

```
ALLOC take_id #mwt1..#mwt4
ALLOC templatestyles #mwt5  src=Plainlist/styles.css
... #mwt6..#mwt13, all Plainlist/styles.css
ALLOC templatestyles #mwt14 src=Module:Infobox/styles.css
```

The nine `Plainlist` styles come from `{{ubl|…}}`, which is the infobox's
`producer` argument. rustoid expands **every `#invoke` argument up front**
(`expand_invoke_args` runs before `expand_invoke`), so the argument's templates
spend their ids before the module has run at all. The service numbers the
module's *own* stylesheet first because Scribunto's `frame.args` is **lazy**: an
argument value is preprocessed the first time the module reads that key, not
when the frame is created. `Module:Infobox` emits its stylesheet before it reads
`frame.args.producer`, so the stylesheet takes `#mwt5` and the `{{ubl}}` styles
come after.

> **Done.** This was implemented in §"Lazy `#invoke` arguments" (the end of this
> file); it is one argument per read, not all at once.

The eager expansion was itself a fix — handing the module the raw source left
`{{If empty|…}}` inside `frame.args` and made modules re-enter the parser — so
this is not a matter of reverting it. The faithful shape is to answer each
`frame.args` read as a deferred request (`FrameRequest`), expanding that one
argument at that moment; the cost is one module re-run per first-read key, which
for a busy infobox is dozens of runs and needs measuring before it lands. It is
recorded rather than started, because it is a change to *when* the module runs,
not to any single pass.

## `PROTECTIONEXPIRY` wants the database's 14 digits, not ISO 8601

`Nobel Prize`'s first difference was, once its protection fact was in the cache,
`Script error: … Module:Effective protection expiry; malformed expiry
timestamp`. The module matches the parser function's answer against
`^(%d%d%d%d)(%d%d)(%d%d)(%d%d)(%d%d)(%d%d)$` — MediaWiki's raw DB form,
`20261128180122` — while the cache stored the `action=query&prop=info`
`expiry` verbatim, which is ISO 8601 (`2026-11-28T18:01:22Z`).

The trait already documented the DB form (`ProtectionEntry::expiries` is "in
MediaWiki's 14-digit `YYYYMMDDHHMMSS` form, with the literal `"infinity"`"), so
the fetch, not the consumer, was wrong: `pageinfo::title_protection` now
converts through `to_wiki_expiry`, passing `"infinity"`, an empty value and an
already-raw 14-digit string through unchanged. The two cached entries that
already held ISO shapes were rewritten in place rather than re-fetched, since
the transformation is exactly the one the new code performs.

The `Nobel Prize` page did not fall out of the fix alone — the next difference
is the banner's expiry *date* (`until November 28, 2026 at 18:01 UTC`, empty in
rustoid) — but the module now runs, which it did not before.

### Scoreboard

```
Nobel Prize   600 -> 1112
fixture guard 876/896
subset total  4 006 648 -> 4 006 649
```

## The `#invoke` call's own arguments were not trimmed

`Unix`'s first difference was a stray `<span about="#mwt5"> </span>` where the
service has nothing. It reduces to `{{Infobox OS}}` alone, and the template
opens with

```
{{ {{{|safesubst:}}}#invoke:Unsubst||date=__DATE__|$B=
{{Main other|…}}
```

— a `$B` whose value begins with a newline. `Module:Unsubst` returns
`frame.args['$B']`, so the newline survives into the module's output and rustoid
rendered it as a whitespace span carrying the transclusion's `about`.

The trim already existed, but only on one of the two argument paths:
`frame_args_to_lua` trims a **parent frame's** named values (the `a46b172` fix
for `Module:Autotaxobox`), while `expanded_arg_pair` — the `#invoke` call's
*own* arguments — passed the value through raw. Both now trim, which the
oracle confirms is the rule: `{{#invoke:String|len|  ab  }}` answers `6`
(positional, untrimmed) and `{{#invoke:String|len|1=  ab  }}` answers `2`
(named, trimmed).

The size effect was large and one-sided: `Bicycle` fell 618 KB -> 469 KB
because most templates on the page carry the same `$B=\n…` shape, and the
untrimmed newline had been making rustoid emit a *second*, raw-wikitext copy of
whole paragraphs (`{{sfn|Wilson|1973|p=82}} They are also used professionally
by …`) that the rendered copy already covered. The first difference moved
backwards only because the media gap sits before the affected region.

### Scoreboard

```
Unix              1736 -> 4772
fixture guard     876/896
subset total      4 006 649 -> 3 839 039   (the drop is removed duplicated
                                            raw wikitext, not content)
```

## A module receives a substituted argument, not its source range

The fresh 82-page corpus run surfaced `function not found: lang: error
converting Lua nil to function` on **16 pages**, and it reduced to

```
A{{#invoke:Lang|{{{fn|lang}}}|fr|bonjour}}   -> ok
B{{Lang|fr|bonjour}}                        -> Script error: function not found: lang
```

`Template:Lang` is `<includeonly>{{#invoke:Lang|{{{fn|lang}}}}}</includeonly>`,
and `Module:Lang` does export `lang`. The name handed to `module_function` was
not `lang` at all — a trace showed `function="{{{fn|lang}}}"`.

`argument_value_or_source` was the cause. It asked `argument_value_text` only
for a `KeyValue::Tokens` value and, for everything else, fell through to
`kv_value_source`, which prefers the recorded **source range**. But
the tokenizer already produced the `{{{fn|lang}}}` argument as a token list, and
`attribute_transform_manager` had substituted it back to a `KeyValue::Str("lang")`
— at which point the source range is the *unsubstituted* text. So the module was
handed `{{{fn|lang}}}` where it must see `lang`.

The rule is now spelled out in the function: a `Str` value **is** the argument
text (the tokenizer leaves a plain value as one, and the transform manager folds
a substituted reference back to one), so it is returned directly; only a token
list can need the source, because its text is not always exactly representable.

This is a *content* fix, not a size one: the subset total rose 3 799 716 ->
3 804 788 because the module now produces its `<i lang="fr">…</i>` and tracking
category instead of a script error.

## The remaining shapes, and two reductions worth keeping

### Fresh 48-page corpus measurement

The built-in corpus (`--corpus default`, 48 entries, 3 stalled) is the widest
instrument available, and the `lang` fix above made it worth re-taking:

```
                        before    after lang fix
score                   0/45      0/45
output (x parsoid)      0.59x     0.60x
lua failures            72/19     57/18     distinct
  function not found: lang   16 pages   0 pages
  Module:Authority control / renderSnak   32   34
pages with literal {{...}}   44/48     44/48
```

`function not found: lang` is gone; `renderSnak` is now the largest cluster (and
grew, because the pages it was masking now reach it). `wikilink`, `media`, and
`nowiki` are the first-difference outcomes for the rest, and **44 of 48 pages
still leak a literal `{{…}}`**, which is the structural blocker behind most of
them rather than any single construct.

Four independent defects stand between the subset and a passing page. None is
reduced to a fix, but two have a minimal input now, which is the part that took
the time.

### The encapsulation merge (`Help:Introduction` 492, `Zebra` 2172)

A page-level run of adjacent transclusions is merged by the service into a
single `mw:Transclusion` wrapper whose `parts` list interleaves template parts
and *string* parts for the literal text between them. The reduce is
`<noinclude>{{pp-semi-indef|small=yes}}</noinclude>{{intro to single}}`: the
service serves one `<p about="#mwt4">` with
`parts:[pp-semi-indef, "</noinclude>", intro-to-single]`, while rustoid emits
only the second. `WrapSectionsState::collapseWrappers` is the pass that builds
those mixed `parts` (the fetched source is quoted in the earlier section), but
it is reached from `resolveTplExtSectionConflicts`, and which pair of wrappers
lands in one section range is not something reasoning has pinned down here.

Worse, on the full page rustoid does not merely omit the merge — it emits the
`pp-semi-indef` wrapper *near the end of the body* (byte 5163 of 20934) instead
of at the head, so its position is wrong as well as its grouping. The reduced
input reproduces the wrong *nesting* (the padlock ends up a child of the
`intro to single` paragraph) but not the displacement, so a bisect inside
`Template:Intro to single` is the next step, not a guess at the merge.

### `#tag` arguments lose their parameter substitution (`Megadeth` 2801, `Zebra`)

`Template:Featured article` -> `{{Top icon|id=featured-star|…}}`, and
`Template:Top icon` is `<includeonly>{{#tag:indicator|<body>|name=<expr>}}</includeonly>`.
Reduced to `{{Top icon|imagename=cscr-featured.svg|id=featured-star}}`, rustoid
produces `"attrs":{"name":" "}` and an `extsrc` that still spells
`{{{image|{{{imagename|{{{1|}}}}}}}}}`; the service has `name:"featured-star"`
and the fully substituted file spec. The name expression contains `{{{id|}}}`,
so the failure is generic: **`{{{…}}}` inside a `#tag` argument is not
substituted**. Zebra's `name:" "` is the same defect, and `Megadeth` only
moved once its link-existence facts were cached.

The reduce pins the two halves exactly. A page-level
`{{#tag:indicator|[[File:{{{1|X.svg}}}|20px]]|name={{{2|foo}}}}}` comes out with
`name:"foo"` (a *plain* named value is substituted by
`attribute_transform_manager`) but `extsrc:"[[File:{{{1|X.svg}}}|20px]]"`: the
positional content is a `wikilink`, and its target is a token list the
transform manager does not descend into — the same shape the `#invoke` argument
path already had to fix. And `{{#tag:indicator|…|name={{#if:…}}}}` shows the
other half: a *nested template* in a `#tag` argument is not expanded before
`pf_tag` runs, because only `attribs[0]` (the target) goes through
`expand_target_templates` and the rest are handed straight to the tag.

An attempt to fix it by running `#tag`'s arguments through
`expand_invoke_args` before `TemplateHandler::process` is **not** in the tree:
it left the wikilink body unexpanded anyway (the argument-reference half of
`frame.expand` does not reach a link target), and it *lost* content — the subset
fell 3.80 MB -> 3.23 MB because `#tag` also carries bodies (`ref`, `nowiki`,
`pre`) whose arguments must not be pre-expanded the way a template's are. The
right shape is smaller and more careful than that: it has to tell the body
argument of a *raw-content* tag from one that is wikitext, which needs the
extension's `hasWikitextInput` flag to reach `pf_tag`. Recorded, not retried
blind.

### Lazy `frame.args` (above) and the file-info cache

`Quicksilver` (2732), `Bicycle` (6979), `List of sovereign states` (4009) and
`Sundial` (1897) all now halt at a *fact* rather than a parser bug: the last two
at a missing file's media info, `Quicksilver` at the id order that lazy
`frame.args` fixes, `Bicycle` at the same media gap. `AddMediaInfo` fetches
through `DataSource::get_file_info`, whose comparison-harness implementation was
a stub returning `None`, so offline media was always `mw-broken-media`.
Populating it needed a thumbnail width, which the trait did not carry
(`getFileInfo` in PHP takes `$width`). That is now done; the account is in *The
file-info cache* below, and the two pages blocked here (`Sundial`, `List of
sovereign states`) are blocked on a *populated* cache rather than on the
interface.

## The file-info cache: `missing` is not "no file"

The first implementation of `get_file_info` bailed out with `Ok(None)` as soon
as the API said `missing: true`. For a file with a local description page that is
harmless, and that is why the bug survived review: on Wikipedia most images *are*
local, so the branch looks like the ordinary "file does not exist" path. But a
**Commons-shared** file is reported `missing: true` (there is no local
description page) *while carrying a full `imageinfo` array*:

```json
{"title":"File:Left side of Flying Pigeon.jpg",
 "missing":true, "known":true, "imagerepository":"shared",
 "imageinfo":[{"thumburl":"…/250px-…","thumbwidth":250,"thumbheight":167, …}]}
```

The presence of `imageinfo`, not the `missing` flag, is what says the file
exists — Parsoid's `DataAccess` reads the array. Treating `missing` as the signal
made the harness cache a `null` for the file, and a cached `null` is a *hit*, so
the wrong answer survived every later run; the cache entry had to be deleted by
hand to reproduce the fix.

### The returned size is the display size

The second half is `handleSize`. It does **not** derive the rendered size from
the requested width; it adopts the `thumbwidth`/`thumbheight` the wiki returned
(`!empty($info['thumburl']) && !empty($info['thumbheight'])` ⇒ use it). The old
Rust code computed the size from the request and scaled by aspect ratio, which
happened to agree with the wiki for simple thumbnails and diverged everywhere
else — most visibly for a **bucketed** thumbnail, where a 180px request returns a
`250px-…` URL whose `thumbwidth` is nonetheless 180. So `FileInfo` grew
`thumb_url`, `thumb_width`, `thumb_height`, and `responsive_urls`, and
`handle_size` now ports PHP's rule directly, including the upscale denial for a
`thumb`/`frameless` *bitmap*.

### URL shape, and where the rewrite belongs

The API returns absolute URLs stamped `utm_campaign=imageinfo`; the served page
carries protocol-relative URLs stamped `utm_campaign=parser` (and a framed `src`
makes that `utm_content=thumbnail_unscaled`). The rewrite is mechanical, but
putting it in `AddMediaInfo` was **wrong**: the parser-test fixtures supply their
own `http://example.com` URLs, and stripping the scheme turned each `<img>` into
a bare `<img/>` and cost 5 media fixtures. The two URL forms are the *data
source's* business, so the rewrite lives on `FileInfo::normalize_api_urls`, and
the API-backed sources (`mw_api`, `compare::fileinfo`) call it while the mock
(rightly) does not.

### The mock has to answer per request

With `handle_size` faithful, `MockDataSource` could no longer hand back one fixed
`FileInfo` and let the parser invent the size: PHP's `MockApiHelper::imageInfo`
scales the file for the requested size, exactly as the wiki does. The mock now
ports `MockApiHelper::transformHelper` (itself a port of `ImageHandler`,
`fitBoxWidth`, and `File::scaleHeight`) plus the core thumbnail-URL and
`responsiveUrls` rules, deriving the `/thumb/<d1>/<d2>/<name>/<w>px-<name>`
layout from the stored raw URL. Without this, five packed-gallery fixtures failed
because `scaleMedia` reads the resolved `<img>` width, which `handle_size` was no
longer supplying from the request.

### Attribute order and the missing `srcset`

`srcset` was never emitted (`srcset()` returned `None`); now it is built from
`responsiveUrls`. The attribute order is also now faithful, because byte parity
is the goal and it is verifiable against the served page: PHP copies `resource`,
then sets `thumbattribs` (`src`, `decoding`, `loading`, `srcset` — the API's order
with `width`/`height` unset), then `alt`, then `lang`, then `data-file-*`, then
the normalized `height`/`width`, then `class`. The 250px `Bicycle` thumbnail now
renders byte-identically, `srcset` and all.

The fixture guard never moved: attribute order is sorted by the harness, so only
the *values* (the returned size, the `src`) had to agree — which is exactly why
the media bugs hid behind a green fixture suite.

## Scoreboard

```
Help:Introduction          416 -> 492
Nobel Prize                600 -> 1112
Megadeth                  1650 -> 2801
Unix                      1736 -> 4772
Quicksilver (film)        1551 -> 2732
Bicycle                   3168 -> 6979 -> 8151
Sundial                   1897        (unchanged; media cache)
List of sovereign states  2771 -> 4009
Zebra                     2172        (unchanged)
fixture guard             876/896
subset total              4 007 778 -> 3 799 716 (0.71x)
```

### After the file-info cache (offline, 138 files cached)

```
score                     0/9
output                    3 879 604 vs parsoid 5 351 722 (0.73x)
by outcome                media 3, extension 5, transclusion 1
lua failures              6 pages, 1 distinct
  Module:Authority control / renderSnak   6
  Module:DecodeEncode does not exist      0  (fetched by the online run)
```

The rustoid total *grew* by ~80 KB: the media renders instead of falling back
to the broken span, so this is the metric getting honest again, not a
regression. `Module:DecodeEncode` was a genuine cache gap and is now filled.

### After the three fidelity fixes

```
score                     0/9
output                    3 846 606 vs parsoid 5 351 722 (0.72x)
by outcome                extension 6, media 2, transclusion 1
lua failures              6 pages, 1 distinct  (unchanged)
Bicycle's first diff       6979 -> 8151, outcome label media -> extension
```

The total *fell* by ~33 KB, all of it the dropped indentation. `Bicycle`'s label
changed because its divergence is now the `about="#mwt199"` on a `<sup
class="mw-ref">` — a Cite marker, hence `extension` — rather than the media
markup inside the classifier's window.

**`Bicycle`'s first difference moved past the image.** At byte 6979 the two
renderings now part on table whitespace, not media:

```
parsoid  <tbody><tr><th colspan="2" class="infobox-above">Bicycle</th></tr>…
rustoid  <tbody>  <tr class="">    <th colspan="2" class="infobox-above">Bicycle</th>…
```

So the old "Bicycle is blocked on media" reading was really "blocked on the
image in the infobox", and behind the image was a whitespace/`class=""` defect
in the table the infobox transcludes. The corpus's outcome label still says
`media` because `classify` scans the 400-char window *around* the divergence,
and the `mw:File` span that opens the infobox image sits inside it while
`<tr>` does not — the label names the nearest loud marker, not the cause.

That table whitespace was itself three bugs in a trench coat; they are unrolled in
the next section, which took the difference on to byte 8151.

## Behind the image: three fidelity bugs the fixtures could not see

Once the media rendered, `Bicycle`'s first difference jumped 6979 → 7791 →
8151 on the strength of three independent defects, none of which the parser-test
fixtures could catch. They are worth listing together because the *reason* the
guard stayed green is the same in all three: the harness normalizes what the
served page is compared verbatim.

### The output serializer was pretty-printing

`HtmlSerializer` emitted `"  ".repeat(depth)` before `p`, `table`/`tr`/`td`/`th`,
lists, `div`, and `hr`. Parsoid's `XHtmlSerializer` emits **compact** markup, and
the served page shows it: the infobox table is `<tbody><tr><th…` with no
whitespace. At depth 0 the indent was empty, which is why the whole lead of a
page matched and the first difference landed at the infobox — the first
indenting element below the root.

The parser-test harness sorts attributes and *collapses whitespace between block
elements*, so both the fixture's expected HTML (which has its own, source-derived
newlines) and rustoid's indented output normalized to the same string. A green
fixture suite was therefore not evidence about output whitespace at all. The fix
removes the indentation entirely; the guard did not move.

### `mw.html`'s `addClass(nil)` is a no-op

Scribunto's `mw.html.lua`:

```lua
function methodtable.addClass( t, class )
    if class ~= nil then … t:attr( 'class', class ) … end
    return t
end
```

An **absent** class adds nothing. Rustoid's `Node:addClass` built a list, skipped
nil entries, and then *always* called `self:attr('class', concat(list))` — so
`addClass(nil)` set `class=""`. `Module:Infobox` calls
`addClass(rowArgs.rowclass)` for every row, and the row class is normally absent,
so every `<tr>` was rendered `<tr class="">` where Parsoid emitted `<tr>`. An
empty attribute is not the same as an omitted one.

### `widthOption` is the wiki's default thumb size, not the first limit

`WikiLinkHandler::renderFile` sizes an unsized `thumb`/`frameless` media at
`SiteConfig::widthOption()`, and `Api\SiteConfig` computes

```php
$this->widthOption = $data['general']['thumblimits'][$data['defaultoptions']['thumbsize']];
```

On enwiki `thumblimits = {0:180, 1:250, 2:400}` and `defaultoptions.thumbsize = 1`,
so the default is **250**, not the 180 that rustoid hardcoded (and not 180 alone,
which was harmless only because the fixture site config really does use 180).
`SiteConfig` gained `width_option()` (default 180, matching
`ParserTests\SiteConfig::widthOption()`), the comparison config derives it from
`siteinfo`, and `siteinfo` now requests `defaultoptions`. The cached `siteinfo`
predated that request, so it was refetched once — a config, not a page revision.

### Where that leaves `Bicycle`

The three fixes moved the first difference from 6979 to **8151**, and what
remains is no longer markup: the infobox reference renders as
`about="#mwt11"` in Parsoid and `about="#mwt199"` in rustoid. That is the
`about`-id allocation order — the same lazy-`frame.args` shape recorded above,
now the single thing standing between `Bicycle` and byte parity.

## The encapsulation merge: a whole-DOM range plan

`Help:Introduction` opened with `<noinclude>{{pp-semi-indef|small=yes}}</noinclude>{{intro to single|…}}`,
and the service served **one** `<p about="#mwt4" typeof="mw:Transclusion">` whose
`data-mw.parts` were `[pp-semi-indef, "</noinclude>", intro-to-single]` — with
`"i":0` and `"i":1` on the two templates. rustoid emitted only the second
template, and the first diff sat at byte 492, inside that attribute.

The shape is PHP's `DOMRangeBuilder::findTopLevelNonOverlappingRanges`. The
`intro to single` markers straddle the paragraph boundary (its start marker
lands *inside* the auto-inserted `<p>`, its end marker after the template's
`<div>`), so its range lifts to the `<p>`'s parent; the `pp-semi-indef` range
lives entirely inside that `<p>`, so its start marker's **ancestor** carries the
outer range and it is *nested*. A nested range is not encapsulated at all: its
markers are dropped and its template object — plus the source wikitext between
the two templates — is recorded into the enclosing range's `compoundTpls`, then
`i` is renumbered across the merged list.

rustoid could not see this. Its encapsulation is a bottom-up walk, one sibling
list at a time, so a marker whose partner is in a different list is invisible to
it until `wrap_flipped_children` reaches their common ancestor — and by then the
inner range has already been encapsulated as its own `<span>`. The fix is a
range *plan* computed once over the whole DOM before any encapsulation, in a new
module-level pass (`compute_range_plan`):

1. walk the tree collecting `mw:Transclusion[/End]` markers with their paths,
   `dsr`, and `data-mw` parts (PHP's `findWrappableTemplateRanges`);
2. pair them by `about`, and for each pair compute the common ancestor and the
   two top-level siblings on the start and end paths (PHP's `findEnclosingRange`);
3. attach each range to the top-level nodes it spans, then reproduce PHP's
   nesting walk and overlap merge, building `compoundTpls` as `subsumedRanges`
   records it (`findTopLevelNonOverlappingRanges`);
4. hand the two facts that need the whole tree back to the existing per-list
   code: which `about`s are absorbed, and the compound `data-mw.parts` each
   surviving range must carry.

`wrap_transclusion_children` now drops both markers of an absorbed range (both
are in one sibling list in the case that matters) instead of encapsulating it,
and takes its `data-mw` from the plan's compound parts — text-split and
`i`-renumbered, not through `serde_json`, which sorts object keys and would lose
the byte comparison. `wrap_flipped_children` uses the same compound when it
transfers a cross-paragraph range onto its target.

**Effect.** `Help:Introduction`'s first difference moved 492 → **1098** on the
strength of the correct `<p>` `data-mw` alone.

### The wrong turn: pre-removing the absorbed markers

PHP removes a nested range's marker metas in `findTopLevelNonOverlappingRanges`,
before `encapsulateTemplates`. Copying that — a pre-pass deleting every absorbed
range's markers by path — **broke two table fixtures**: `{{tbl-start}}…{{tbl-end}}`
tables stopped putting `typeof="mw:Transclusion"` on the `<table>` and put it on
the `<tbody>` instead. Deleting the markers changed the *enclosing* range's
`range_end` scan, and `table_body_content_target`'s `well_balanced` test flipped
with it. The markers are not independent data: the enclosing range is recomputed
from the DOM in rustoid, while PHP carries a precomputed `DOMRangeInfo` through.
The pre-pass was reverted; letting each sibling list drop the absorbed markers
as it reaches them achieves the same DOM without moving the enclosing range. The
guard is back at 876/896.

### Two rules for `<noinclude>`: a value keeps it, a target strips it
The next difference (1098) was the `lead` argument's `wt`: the service recorded
`…the basics<noinclude>, and each tutorial…quickly.</noinclude>`, rustoid recorded
it with the tags gone. `prepare_tpl_param_infos` was running
`strip_include_directives` over every argument value. That is the *target* rule,
not the value rule — the transform endpoint settles both:

```
{{1x|a<noinclude>X</noinclude>b}}       → params.1.wt = "a<noinclude>X</noinclude>b"
{{1x|a<includeonly>X</includeonly>b}}   → params.1.wt = "a<includeonly>X</includeonly>b"
{{1x|a<onlyinclude>X</onlyinclude>b}}   → params.1.wt = "a<onlyinclude>X</onlyinclude>b"
{{#if:<includeonly>X</includeonly> |y|n}}→ target.wt  = "#if: "        (whole gone)
{{#if:a<noinclude>X</noinclude> |y|n}}  → target.wt  = "#if:aX "       (tags gone)
```

A template argument's `wt` is the source **as written**, tags and all; only a
parser-function target goes through the preprocessor's include handling. Dropping
the strip from the value path moved the first difference 1098 → **5161** and
held the guard at 876/896.

### What is left on `Help:Introduction`

At 5161 the divergence is the `pp-semi-indef` range's own output, now bare inside
the `<p>` as intended. Two defects remain there, neither about the merge:

- rustoid leaves an empty `<span typeof="mw:Nowiki"></span>` where the service
  has none — the absorbed range's marker-shape target, which Parsoid never
  builds because a nested range is never encapsulated;
- the `<meta typeof="mw:Extension/indicator">` has lost its `about`. Parsoid
  gives it `about="#mwt3"` (the pp invocation's id, assigned when the extension
  was emitted inside the expansion, not at encapsulation). rustoid only ever set
  that `about` during encapsulation, so skipping the nested range drops it.

Both are downstream of *how many* ids the two engines hand out and in what
order — the lazy-`frame.args` shape that also blocks `Bicycle` — so the merge is
the last purely-structural piece of this page.

### Scoreboard after the merge

```
Help:Introduction         492 -> 5161
Nobel Prize               1112  (unchanged)
Megadeth                  2801  (unchanged)
Unix                      4772  (unchanged)
Quicksilver (film)        2732  (unchanged)
Bicycle                   8151  (unchanged)
Sundial                   1897  (unchanged)
List of sovereign states  4009  (unchanged)
Zebra                     2172  (unchanged)
fixture guard             876/896
subset total              3 846 606 -> 3 818 304
```

Every other page's first difference is byte-identical to the previous run, so the
merge changed only `Help:Introduction`'s head. The subset total fell ~28 KB, and
almost all of it is `List of sovereign states` (718 370 → 692 977): the absorbed
ranges' eager wrapper spans are gone. That is a *structural* drop, not lost
content — a text-only comparison of the two renderings (tags stripped, entities
decoded) has the new one 326 bytes **larger**, not smaller.

## The id order: `Bicycle`'s `<sup>` is not the eager-args case
The follow-up on the todo list was the lazy `frame.args`. `Bicycle` is the page
that motivated it: its infobox reference renders `about="#mwt11"` in Parsoid and
`about="#mwt199"` in rustoid. The two id sequences, in document order of first
appearance, agree exactly up to the ninth:

```
parsoid  #mwt1 #mwt2 #mwt3 #mwt4 #mwt5 #mwt6 #mwt8 #mwt9 #mwt10 #mwt11 …
rustoid  #mwt1 #mwt2 #mwt3 #mwt4 #mwt5 #mwt6 #mwt8 #mwt9 #mwt10 #mwt199 …
```

So the *order* is right; the *counter* has run 188 ids further ahead by the time
the `<sup>` is numbered. The obvious suspect is the eager `#invoke` argument
expansion that explains `Quicksilver` above, so it was measured rather than
assumed: forcing `expand_invoke_args` to a no-op moves the `<sup>` from
`#mwt199` to **`#mwt204`** — five ids, in the *wrong* direction. The eager
argument expansion is **not** what costs the 188 here. (`Quicksilver`'s nine
`Plainlist` styles are a real instance of it; `Bicycle` is not.)

The 188 ids belong to infobox rows that appear *after* the caption in the
document, numbered before the caption's `<ref>`. That is an allocation *phase*
difference, not an ordering one: Parsoid numbers the `<ref>`'s output when the
caption argument is expanded, while rustoid numbers it in the later Cite pass,
by which time `Module:Infobox` has already run and spent the rows' ids. The fix
belongs where extension output gets its `about`, not in `frame.args` — which is
also why the earlier section's remedy, recorded against `Quicksilver`, is not the
one `Bicycle` needs.

This is left recorded rather than started: it is a change to *when* an extension
node is numbered, it touches every extension on every page, and doing it blind —
against a diagnosis the measurement just contradicted — is how the last three
"fidelity bugs" got shipped behind a green fixture guard.

## The extension-numbering phase: extensions are numbered in TT2
The measured facts above said the gap was a *when-is-an-extension-numbered*
difference. The PHP settles both halves.

**Where the id comes from.** Every non-`nowiki` extension gets its `about` in
`ExtensionHandler::onExtension`, and only there — Cite does not number its own
refs (`Cite/src/Parsoid/RefTagHandler.php` only *reads* `about`):

```php
// ExtensionHandler.php, in onExtension, for $extensionName !== 'nowiki'
$about = $env->newAboutId();
$n = $firstNode;
while ( $n ) { $n->setAttribute( 'about', $about ); $n = $n->nextSibling; }
```

**When.** `ExtensionHandler` sits in `TokenTransform2`, right after
`TemplateHandler`, before `AttributeExpander`, and
`TokenHandlerPipeline::processChunk` runs each transformer over the **whole
chunk** before the next one:

```php
foreach ( $this->transformers as $transformer ) {
    $tokens = $transformer->process( $tokens );
}
```

So at each level the chunk's templates are expanded and numbered first, and the
extensions are numbered after — regardless of their order in the source. The
transform endpoint confirms it: `<ref>a</ref>{{1x|b}}` numbers the *template*
`#mwt1` and the ref `#mwt2`, and so does `{{1x|b}}<ref>a</ref>`.

### The fix
`Parser::expand_templates` is TT2, and it already resolves `<templatestyles>` and
`<indicator>` inline for exactly this reason. Its post-pass now numbers every
`ref`/`references` extension token in the chunk's output after the template loop,
writing the id onto the token's `about` attribute so it reaches the DOM. The Cite
pass then *consumes* that id instead of allocating its own. The post-pass runs on
`out`, so a chunk nested in a template (a template body, an `#invoke` argument)
is numbered by its own recursive call first, and a token that already carries an
`about` is left alone.

Only the extensions whose output consumes the id are numbered here.
`<ref>`/`<references>` keep it on the `<extension>` element Cite reads.
`<nowiki>` is lean markup with no `about`. `<pre>`/`<style>` and the rest are
rebuilt from their rich `data-mw` attribs (`extension_kv_attrs`), which do not
carry a token attribute, so numbering them here would spend an id the output
never shows — a second drift. They are left to a follow-up.

**Effect.** `Bicycle`'s `<sup class="mw-ref reference">` is now `about="#mwt11"`,
matching the service exactly, and its first difference moved **8151 → 11836**. No
other subset page's first difference moved, and the guard held at 876/896.

### The guard cannot see any of this
The parser-test harness strips `about` in `normalizeOut` (`harness/mod.rs`, the
same list as `prefix`/`rev`). So a change that only moves `about` ids is invisible
to all 896 fixtures — which is why `<pre>` has been missing its `about` for a long
time without a single failure, and why this fix had to be validated against the
served page and the transform endpoint. The new unit test asserts on the rendered
HTML, and it was checked to *fail* when the post-pass is disabled.

### Two gaps left on `Bicycle`
- **One id fewer than the service** on a reference nested in a template:
  `{{1x|X<ref name=r>b</ref>}}{{1x|a}}` numbers the later templates `#mwt3`/`#mwt4`
  where the service has `#mwt4`/`#mwt5`. The reference's own `about` is
  overwritten by encapsulation either way, so only the counter position shows it.
  Recorded, not asserted.
- **The new first difference (11836) is a fact drift, not a bug.** rustoid renders
  `[[Electric bicycle]]` with `class="mw-redirect"`; the service does not. The API
  says the page *is* a redirect today, but the cached `info:Electric bicycle` was
  fetched at `epoch:1790590761`, seven days after the `html:Bicycle` baseline at
  `epoch:1789975300`, so the redirect was created between the two. Facts are not
  revisioned, so an old baseline can be compared against a newer fact; the only
  clean repair is to re-pin the baseline, which the corpus rules forbid casually.

### Scoreboard after the extension-numbering phase

```
Bicycle                   8151 -> 11836
Help:Introduction          5161   (unchanged)
Nobel Prize                1112   (unchanged)
Megadeth                   2801   (unchanged)
Unix                       4772   (unchanged)
Quicksilver (film)         2732   (unchanged)
Sundial                   1897   (unchanged)
List of sovereign states  4009   (unchanged)
Zebra                     2172   (unchanged)
fixture guard              876/896
```

## The age guard: a difference can be wiki drift, not a bug

The 11836-byte first difference above is not a parser bug. rustoid renders
`[[Electric bicycle]]` with `class="mw-redirect"` and the service does not, but
the cached `info:Electric bicycle` was fetched *seven days after* the
`html:Bicycle` oracle. The redirect was created in between. Nothing in the run
said so: it printed a byte offset and left the reader to assume rustoid was
wrong.

That is a class of error the harness could not previously distinguish. The
**oracle is pinned to a revision**; the **facts** a render leans on — a title's
existence and redirect-ness (`PageInfo`), its protection (`Protection`), a file's
size (`FileInfo`), an entity — are not revisioned. The cache holds whatever the
wiki answered when the entry was fetched. If that was a different day from the
oracle, a difference between the two renderings may be drift, and re-pinning the
fact would erase it rather than any code change fixing it.

### What is measured
Every cache hit during a render is now observed through `CachedDataSource`
(`FactAges`), keeping the **newest** entry it saw. The harness already records
`fetched_at` on every entry (it was added for timelining, and every fact kind
already writes it), so this costs nothing to observe. After the render,
`FactDrift::detect` compares that newest fact against the oracle's own
`fetched_at`: a fact more than `FACT_TOLERANCE_SECS` (**60**) newer marks the
comparison **suspect**.

Sixty seconds, not more: a page's oracle, wikitext and render are fetched within
seconds of each other, so anything beyond a minute apart was fetched in a
different session — which is exactly when a fact can have moved on from the
revision the oracle rendered. Only facts **newer** than the oracle count; an
older fact cannot explain a difference against the later, authoritative oracle.

### It is reported, not hidden
The page still counts as a difference — the score is unchanged — because the
drift is a *caveat* on a difference, not a pass. It is named and listed:

```
possibly stale facts (8 of 9 compared — the difference may be wiki drift):
  Bicycle                     facts 7d newer than the oracle (newest: file:File:Banana-bike.jpg@w250)
```

and the failure line carries `(suspect drift)`, and a single-page run prints
`suspect: …` under the verdict. The message names the *newest* fact, which is
illustrative, not necessarily the culprit — the actionable part is the age.
Deliberately a distinct signal rather than a `Skipped`: the user wants to see it,
not have it disappear from the count.

### What the guard says about the current cache
Run offline over the 9-page subset, **8 of 9** pages are flagged `7d newer`. That
is accurate and it is the cache's state, not a design flaw: the `html:` oracle
baselines were pinned at `epoch:~1789975300`, while the `info:`/`prot:`/`file:`
entries were populated days later, on demand, as the session filled gaps. It
means the current subset scoreboard is measured against a baseline a week older
than the facts around it, and should be read with that in mind — which is exactly
the honesty the guard exists to force. A clean measurement needs a cache whose
facts and oracle were populated together.

### Guarded by tests
Six unit tests cover `FactDrift::detect` (newer/within-tolerance/older, newest
wins, missing `fetched_at`, missing oracle time) and one covers the scoreboard
section; the display format is asserted so the report cannot silently change
shape.

## Two Lua fidelity bugs behind one navbox
The `mw.wikibase.renderSnak` gap had been on the list for sessions as "not on the
critical path". It is on the path to *reading* the pages, though: it made
`Module:Authority control` stop with `attempt to call field 'renderSnak' (a nil
value)` on **6 of the 9 subset pages**, so their whole authority-control navbox
was replaced by a `Script error`. Both bugs below are in the Lua engine, and both
were found by opening the served bytes rather than trusting a summary.

### `mw.html`: styles are a separate list, not a `style` string
Rustoid's `mw.html` merged `:css()` declarations into the `style` attribute as
`name: value;`. Scribunto does not do that. It keeps declarations in their own
list and serializes them at build time as `name:value` joined by `;` — no space
after the colon, no trailing semicolon:

```lua
-- mw.html.lua, methodtable._build
if #t.styles > 0 then
    table.insert( ret, ' style="' )
    ... table.insert( css, prop.name .. ':' .. prop.val ) ...
    table.insert( ret, table.concat( css, ';' ) )
```

So a navbox group cell built with `:css('width', '1%')` is served as
`style="width:1%"`, and rustoid emitted `style="width: 1%;"`. The same applies
to several details the split makes possible and the merged string could not:
`attr('style', v)` **replaces** everything `:css`/`:cssText` added, a repeated
property replaces the earlier one, and `cssText` adds a raw declaration. Scalar
characters 58 (`:`) and 59 (`;`) are escaped inside a declaration by
`cssEncode`, which the merged form never did. Self-closing tags (`<br />`) were
also rendered as `<br></br>`.

The port now mirrors `mw.html.lua`'s structure: an ordered `{name=,val=}`
attribute list, a separate declaration list, and `htmlEncode`/`cssEncode` at
build. `addClass` is **one** argument, as Scribunto's is (`addClass('a','b')
ignores `b`); the invoke test that passed two is corrected. Effect on the
subset: **+20 818 bytes** (3 818 259 -> 3 839 077), with every navbox/infobox
`style` now byte-identical. It moves no *first* difference — the first
difference on every subset page is above the first navbox — so the scoreboard
still reads 0/9. That is the honest measure, not a lack of change.

### `mw.wikibase.renderSnak`
The method did not exist. The PHP chain is short and unambiguous:
`WikibaseLibrary::renderSnak` -> `SnakSerializationRenderer::renderSnak` -> the
`DataAccessSnakFormatterFactory` formatter with
`TYPE_ESCAPED_PLAINTEXT`. That type is a `BinaryOptionDispatchingSnakFormatter`
whose whole point is one exception: a `url` snak is formatted plain and returned
**unescaped**, everything else is run through
`SnakFormatter::FORMAT_PLAIN` and then `wfEscapeWikiText`.

`wfEscapeWikiText` lives in `GlobalFunctions.php` (mediawiki/core) — it is *not*
`Sanitizer::escapeWikitext`, which no longer exists; and it is a different
function from Parsoid's html->wt serializer escaper. It is a replacement table
(`&`->`&#38;`, `[`->`&#91;`, `://`->`&#58;//`, …) plus first/last-character
protection (a leading `+`/`-`/`_`/`~`, a trailing `_`/`~`/newline) plus
`\b(protocol):` for the colon-suffixed `$wgUrlProtocols`. It is ported as
`sanitizer::escape_wikitext_text`, with its own tests.

In the cached corpus `renderSnak` is only ever reached with **`string`**
snaks — `P1810` qualifiers, 984 of them, and nothing else — so the formatter
handles `string`/`external-id`/`commonsMedia`/`url`/`monolingualtext` exactly and
returns a datatype whose plain form is computed (`time`, `quantity`, …) as its
raw value rather than a *wrong* one. `renderSnaks`, `formatValue` and
`formatValues` are implemented too, from the same shape.

**Effect.** The `Script error` is gone and the authority-control navbox renders:
`Nobel Prize` now has the same `1` `GND` link and the same `156` `navbox`
occurrences as the service. Its first difference stays at 1112, but the cause
changed — it is now the **protection expiry**, which rustoid leaves empty where
the service has `semi-protected until November 28, 2026 at 18:01 UTC`. That is
the next thread.

### The protection expiry: `formatDate` could not read `@<unix>`
The empty `until ,` was not the fact and not the magic word. Both were already
right: the cache holds `prot:Nobel Prize` = `"expiries":{"edit":
["20261128180122"]}`, and `{{#invoke:Effective protection expiry|edit}}` returns
`2026-11-28T18:01:22`. The loss was one step further along, in
`Module:Protected page`:

```lua
-- Blurb:_formatDate
local success, date = pcall(
    lang.formatDate, lang,
    self._cfg.msg['expiry-date-format'] or 'j F Y',   -- 'F j, Y "at" H:i e'
    '@' .. tostring(num)
)
if success then return date end                        -- no error on failure
```

`mw.language:formatDate` therefore had to read two things rustoid's `format_date`
did not: the **`@<seconds>` Unix-timestamp input** (`parse_date` knew ISO, the
eight-digit form, month names and a bare year, but not `@`), and the format codes
**`"…"`** (a literal run, quotes dropped) and **`e`** (the timezone identifier,
`UTC`). Because the caller wraps the call in `pcall` and returns its own value
when it fails, the failure was *silent*: the `${EXPIRY}` parameter came back
`nil` and the banner read `until ,`. The `@` form now parses with `chrono`, and
`"`/`e` are handled. The served and rustoid banners are byte-identical:

```
This article is semi-protected until November 28, 2026 at 18:01 UTC, due to vandalism
```

**Effect.** `Nobel Prize`'s first difference moves **1112 -> 4269**, and the
whole protection block before it is now exact. The new first difference is an
`about`-id mismatch (`#mwt15` here, `#mwt6` there) on an infobox's templatestyles
— the id-ordering thread again, one spending decision upstream of it.

## An `about`-id trace, and the nine ids it found
That mismatch had only ever been *reasoned* about. The output cannot settle it:
rustoid allocates **392** ids and emits only **309** distinct ones, and an id spent
on a token that is later dropped looks exactly like one whose `about`
encapsulation overwrites. So `new_about_id` now takes a `tag` naming the kind of
thing numbered (a template wrapper, an extension, an expanded attribute, …) and,
under `RUSTOID_TRACE_ABOUT`, logs every allocation. `RUSTOID_TRACE_ABOUT=bt` also
prints a backtrace, which names the *code path* rather than just the kind.

The trace answered the question immediately. rustoid's allocations `#mwt6`..
`#mwt14` — six templatestyles and three refs — are **invisible** (no element
carries the id), and every one of them has the same stack:

```
expand_one_templatestyles <- expand_templates <- expand_template_token
  <- expand_templates <- expand_invoke_args <- expand_template_token ...
```

while the first *visible* one, `#mwt15`, has `expand_invoke <- expand_lua_request`
instead. So the nine ids are spent expanding the **arguments** of an `#invoke`
call — eagerly, up front — and Parsoid spends nothing there because it expands an
`#invoke` argument **lazily**, when the callee first reads it. That is the
recorded lazily-expanded-argument-value family (§"The lazily-expanded argument
value", §"`#invoke` arguments are expanded lazily"), now measured to the id: it
is worth exactly the 9-id gap on `Nobel Prize`'s infobox.

This is a *diagnosis*, not a fix. The earlier measurement stands — simply
disabling `expand_invoke_args` moved `Bicycle`'s `<sup>` from `#mwt199` to
`#mwt204` — so the answer is real lazy expansion, not removal. What the trace adds
is that the eager pass is the whole of the discrepancy at this position, and
which tokens it overspends on.

## Lazy `#invoke` arguments: `frame.args` is expanded on first read

The diagnosis above is now a fix, and the PHP settles the exact shape. The
earlier reading of `PPTemplateFrame_Hash` as "expand every argument on the
frame's first read" was **wrong** — that method, `expandArgs`, does not exist in
the current core. The real code expands **one argument per read**:

```php
// PPTemplateFrame_Hash
getArgument($name)  → getNumberedArgument($name) ?: getNamedArgument($name)
getNumberedArgument($index)  // caches in $numberedExpansionCache, no trim
getNamedArgument($name)      // caches in $namedExpansionCache, trim() after expand
getArguments()               // getArgument() for every key: numbered first, then named
```

Scribunto's Lua side is lazy per key too (`Engines/LuaCommon/lualib/mw.lua`):
`frame.args` is an empty table whose metatable's `__index` calls
`php.getExpandedArgument(frameId, name)` — one argument, cached in the Lua-side
`argCache` — and whose `__pairs` calls `php.getAllExpandedArguments` (i.e.
`getArguments`, all of them). `frame:argumentPairs()` is literally
`return pairs( self.args )`. And `Hooks::invokeHook` expands exactly two things
up front, the ones it must to pick a target at all:

```php
$moduleName   = trim( $frame->expand( $args[0] ) );
$functionName = trim( $frame->expand( $args[1] ) );
unset( $args[0], $args[1] );
$childFrame = $frame->newChild( $args, $title, … );   // the rest: raw PPNodes
```

### The implementation

`Arg` gained a `Lazy(ArgSlot)` variant. An `ArgSlot` names the raw argument —
which list (`Call` for the `#invoke` call's own arguments, `Parent` for
`frame:getParent().args`) and its index — and carries the wikitext as written
for the no-data-source echo. The parser builds the slots; `expand_invoke` holds
the raw `KV` lists beside them.

`build_args_table` materializes only *answered* slots as raw keys, and installs
three metamethods for the rest:

- `__index` resolves the read key (both spellings, `1` and `"1"`) against the
  unresolved slots. A hit records `FrameRequest::ExpandArgs` for **every**
  still-unresolved slot (see below) and raises the `NOT_CACHED` signal; the host
  expands them (the same code the eager path used) and the module is re-run.
- `__pairs` — reachable because rustoid already back-ports `__pairs`/`__ipairs`
  — records **every** still-unresolved slot at once, numbered before named as
  `getArguments` does, so `pairs(frame.args)` costs one round rather than one per
  argument.
- `__ipairs` reproduces Scribunto's `argsInext`: read `1, 2, 3, …` through the
  same lazy resolution, stopping at the first absent key.

`invoke`'s host closure now returns `Vec<(String, String)>` — a request may
carry several answers — and the round guard is keyed per answer, so a `pairs`
over 108 arguments is still one settled round.

#### One read expands the whole argument set

The first version of `__index` raised for the *one* key that was read. That is
faithful to Scribunto's observable `argCache` behaviour, but it is ruinous here:
rustoid cannot resume a call that raised an error, so `run_once` builds a fresh
`LuaEngine` and re-executes the module from the top for every round. A module
that walks `frame.args` therefore ran its whole body once per argument —
`Module:Citation/CS1` does exactly that for the ten fields of a `{{cite}}`.
`2024 Summer Olympics` paid **2406 `ExpandArgs` rounds** out of 3154, and the
page rendered in **47.7 s**.

Scribunto has no such cost, and not because of continuation magic: it never
*reads* argument by argument. `getArguments()` expands the frame's whole
argument list as a set (numbered before named), so the analogue of "the host
expands on first read" is "the host expands *all of them* on the first miss".
`__index` now records every remaining slot at once, in `getArguments` order,
and raises. Later reads of the same round are already covered because the request
is recorded once (`pending.borrow().is_empty()`), and the errors after the first
unwind the call anyway.

### Effect

The same page now takes **1032 rounds (786 `ExpandArgs`) and 15.4 s** — a 3.1×
round reduction and a 3.1× wall-time speed-up, with the first difference
*unmoved* at byte 8910 (`0.47%` of parsoid), because that difference is the
unexpanded-`<ref>`-body `about` drift, not id ordering. The fixture guard holds
at 877/896 and the `about` ids for the page's first marker are unchanged
(`#mwt8`, against the service's `#mwt10`).

`Nobel Prize`'s infobox templatestyles moved from `#mwt15` to `#mwt9`: the
nine-id gap the trace found is down to **three** at that point (the last three
are a different mechanism, fixed in §"The three ids left" below). No other
page's first difference moved, the fixture guard held at 876/896, and the corpus
total is byte-identical to four decimals (`3 839 090` vs `3 839 107` before — the
17 bytes are that id's digits). That is the honest measurement: this fix changes
*id allocation order inside an `#invoke`*, and almost every page's first
difference sits elsewhere.

### The three ids left, and why they are a different fix

Tracing them names the mechanism exactly. `Module:Infobox` reads its `data8`
(country) and `data9` (presenter) arguments in `parseDataParameters`, *before*
`loadTemplateStyles` emits its own `Module:Infobox/styles.css`. Each of those
arguments contains a `{{Plainlist}}`, so expanding them allocates
`Plainlist/styles.css`'s `about` ids. In rustoid that happens at argument-expansion
time, in module *read* order — so the Plainlist styles take `#mwt6`/`#mwt7` and
the module's own stylesheet gets `#mwt9`. The service has `Module:Infobox/styles.css`
`#mwt6` and Plainlist `#mwt7`: **document order in the module's output**, where
`loadTemplateStyles() .. root` puts the base style before the table that holds
the `data8`/`data9` rows.

That is the extension-numbering phase again (§"The extension-numbering phase"),
from its unnumbered side. `ExtensionHandler::onExtension` numbers every
non-`nowiki` extension in TT2, *after* `TemplateHandler`, over the whole chunk —
so a chunk's extensions are numbered after its transclusions. Two things in
rustoid violated that, and both are now fixed.

#### The templatestyles post-pass

`expand_one_templatestyles` used to take the `about` id where the walk reached the
tag, which is document order *against* the templates — the transform endpoint
rejects it:

```
<templatestyles src="Plainlist/styles.css"/>{{Center|b}}   (rendered as Sandbox)
wiki   : Center #mwt1, templatestyles #mwt2
rustoid: templatestyles #mwt1, Center #mwt2      (before this fix)
```

Now the resolved stylesheet is stashed in `pending_styles` keyed by its fragment
id, and `number_style_placeholders` — the same shape as the `ref`/`pre` post-pass
that was already there — walks the chunk's output at the end of
`expand_templates`, assigning ids in document order and building the `<style>`
fragment then. A nested chunk still numbers its own first (a stylesheet inside a
template body is `#mwt2` behind the wrapper's `#mwt1`, on both sides), because
the post-pass runs per chunk. A pinned test
(`a_stylesheet_before_a_template_is_numbered_after_it`) fails on the old order.

#### Arguments expand without spending extension ids

The second half is `arg_expansion` (a counter, not a bool, so nesting is exact).
Scribunto's `frame.args` hands a module the argument's *text*, and the templates
in it are expanded by the preprocessor — which numbers no extensions. So while
`expand_invoke_args` runs, the extension post-passes are skipped: no `about` is
spent on a token that is about to be rendered back to text and discarded. The
instrumented trace settled it: `data8`/`data9`/`presenter`/`country` were each
spending one or two ids during argument expansion, all discarded (`presenter`'s
text came back as the *source* `{{Plainlist|…}}`, which the module's output then
re-expanded and numbered a second time). Resolving is left alone — the rendered
text is the argument's source fallback either way — and the module's own
`frame:extensionTag` calls still number, because they run from `expand_invoke`,
not from an argument.

### Effect

`Nobel Prize`'s infobox stylesheet is now `#mwt6`, matching the service, and its
first difference moved **4269 → 5926** — the id mismatch is gone and the next
difference is unrelated (an image: rustoid renders `typeof="mw:File"` +
`data-mw` where the service has `<a>`-wrapped `mw:File/Frameless`).
`Quicksilver` also moved, **2732 → 4213**, on the same mechanism. No page's first
difference regressed:

```
Help:Introduction          5161   (unchanged)
Quicksilver (film)         2732 -> 4213
Unix                       4772   (unchanged)
Zebra                      2172   (unchanged)
Sundial                    1897   (unchanged)
Bicycle                   11836   (unchanged)
Nobel Prize                4269 -> 5926
List of sovereign states   4009   (unchanged)
Megadeth                   2801   (unchanged)
fixture guard             876/896
```

All four order probes agree with the service now (style-before-template,
template-before-style, style in an argument, style in a template body), and the
whole workspace suite and clippy are clean.

## A wider corpus: all 48 cached article baselines

The 9-page subset is too narrow to see whether an id fix is broadly safe, so the
measurement was widened to **every cached Parsoid baseline** (48 titles, 40
compared, 6 skipped, 2 stalled). Offline, `--corpus /tmp/wide.corpus`:

```
score: 0/40 compared (0.0%), 6 skipped, 2 stalled
output: parsoid 49 802 466 bytes, rustoid 33 215 208 bytes (0.67x)

by outcome: extension 20, transclusion 11, media 4, wikilink 3,
            data-mw 1, nowiki 1, skipped 6, stalled 2
```

No page passes, and the ratio is worse than the subset's (0.67× vs 0.72×)
because the wide set is full of huge articles whose templates this rustoid
still expands short — but the useful fact is *where* they differ. **Not one
page's first difference is an `about` id** (the harness has a `marker-ids`
category for exactly that; the bucket is empty). The id-ordering thread is
clean at the first difference across all 40.

The smallest differences fall into a handful of *unrelated* shapes, and none is
this session's work. Reading the differing bytes rather than the category label
is what shows it — several were first mis-filed here:

| first diff | page | actual shape |
|---|---|---|
| 544 | Zebro | `Short_description_…`: `is_different_from` vs `with_empty_Wikidata_description` |
| 546 | Grand Theft Auto V | an empty `{{Pp}}` line: service `<p class="mw-empty-elt" id="mwAw">` wrapping the nowiki transclusion, rustoid a bare `<span class="mw-empty-elt" about="#mwt2">` |
| 572 | Polio vaccine | same empty-elt line, but rustoid's `<p id="mwAw">` is missing the class |
| 578 | Python (programming language) | `Short_description_matches_Wikidata` is on both sides; the diff is the same empty-elt wrapper |
| 588 | Chernobyl disaster | same empty-elt wrapper |
| 600 | ISO 3166-1 alpha-2 | same, missing `class="mw-empty-elt"` |
| 642 | COVID-19 pandemic | same, missing `class="mw-empty-elt"` |
| 896 | Doom (1993 video game) | indicator `attrs.name` `featured-star` vs `" "` (§ the indicator-name bug) |
| 939 | World War II | same empty-elt wrapper, on a `<span>` |
| 976 | Hydrogen | a disambiguation link is missing `class="mw-disambig"` |
| 1492 | Isaac Newton | same missing `mw-disambig` |
| 1521 | Albert Einstein | a hatnote `<span about="#mwt2">` is missing ` id="mwBA"` |
| 5926 | Nobel Prize | image: `typeof="mw:File"` + `data-mw` where the service has an `<a>`-wrapped `mw:File/Frameless` |

So the next page to fall is the **empty-`{{Pp}}` line's `mw-empty-elt` wrapper**
(seven of the ten smallest), then the missing `mw-disambig` class and the
hatnote `id`. `Module:Math` differs at byte 1 for a different reason: the
Module-namespace page renders its *source* where the service renders the
documentation tree — a namespace/content-model gap of its own. Facts for 39 of
40 are 7 d newer than their pinned baselines, so several of these may be drift
(the harness flags them `suspect`); only a re-pin can settle those.

## The `mw-empty-elt` wrapper was a cache bug, not a parser bug

Seven of the ten smallest wide-corpus differences were one shape: rustoid rendered
the empty `{{Pp}}` line as a bare `<span class="mw-empty-elt">`, the service as
`<p class="mw-empty-elt">…</p>`. `CleanUp::handleEmptyElements` only marks a
paragraph empty when its children are *rendering-transparent*, and rustoid's
child was a `<a rel="mw:WikiLink" href="./Template:Cs1_config">` — a red link to
a template that plainly exists. So the paragraph was not "empty" and never got
the wrapper or the `id`.

The template exists: `Template:CS1 config` (rev 1305415940) is real, and
`Template:Cs1 config` is a **redirect** to it. rustoid asked for the first and
got the second's wikitext. The cause was not title resolution at all — it was the
**cache filename scheme**.

### The defect

Bodies are stored as `pages/<key>.txt`, and the "current" scheme escaped only
`/`, `\` and NUL, leaving case alone:

```rust
key.replace(['/', '\\', '\0'], "_").replace("..", "__")
```

Two properties of that are wrong, and both silently serve one page's wikitext for
another:

- **`/` and `_` collide on *every* filesystem.** `Module:Citation/CS1` and
  `Module:Citation_CS1` are different keys with one file between them. Measured
  on the corpus cache: **530 index-key groups** share a sanitised stem.
- **Case collides on a case-insensitive filesystem** (macOS's default). The cache
  itself reported *no* two files differing only by case — because the filesystem
  does not permit them — while the *index* held 34 case-foldable key groups.
  `Template:CS1 config` and `Template:Cs1 config` shared `tpl:Template:CS1
  config.txt`, last write wins.

Most such groups are harmless: the two keys name the same page (a redirect alias,
or a title the API normalised), and one body genuinely serves both — 549 of the
563 groups carry a single revision. But **14 groups carry two different
revisions**, i.e. two different pages, and the shared file holds only one. The
surviving body under `tpl:Template:CS1 config.txt` was the redirect
(`#REDIRECT [[Template:CS1 config]]`), which is why the paragraph above was a red
link.

Wrong turn worth recording: this was first written up as a *case-policy* gap —
template transclusion on enwiki supposedly folding case after the first letter,
which rustoid did not implement. It was not. Reading the cache body file (81
bytes of `#REDIRECT`) is what exposed the collision; `index.json` recorded two
entries with different revisions and looked perfectly healthy.

### The fix

`escape_body_stem` now maps a key to a filename **injectively**, escaping only
what would otherwise lose information, as `%XX` uppercase hex:

- `/`, `\`, NUL (cannot be in a filename at all);
- ASCII uppercase (folded by a case-insensitive filesystem);
- every non-ASCII byte (same folding reason, and no normalisation form assumed);
- `%` and `~` (so the escapes stay unambiguous, and `~` stays reserved).

`%XX` keeps titles greppable (`grep -i cs1` still finds the file) while making
`Template:CS1 config` and `Template:Cs1 config` distinct on *any* filesystem. A
key whose escaped stem would overflow the ~255-byte filename limit is truncated
and given a `~` + FNV-1a hash suffix; such a stem is not decodable and `reindex`
ignores it. The longest key in the cache escapes to 203 bytes, so the limit is
headroom rather than a live concern.

The two older schemes are still *read* (`locate_body` tries current, then the
`_`-escaped form, then the `__`-colon form), because a corpus cache is hours of
rate-limited fetching and moving files is not worth it. Only writes use the new
scheme.

Genuinely-colliding entries (the 14 groups) cannot be recovered from disk —
nothing records which page the surviving file belongs to — so they are dropped
and re-fetched. `WikiCache::drop_colliding_bodies` does this and is exposed as
`--repair-cache`; it is conservative, skipping any group whose members already
have a file under the new scheme, so it is a no-op on a healthy or repaired
cache. Run against the real cache: **30 entries dropped** (exactly the 14
groups), 7928 → 7898, then `--reindex` recovered 960 bodies that earlier
interrupted runs had left unflushed.

### Result

`Template:CS1 config` holds the real `<nowiki/><!--…-->` template again, and
COVID-19 pandemic's first difference moved from the empty-`p` wrapper to **byte
962** — the wrapper's `class="mw-empty-elt"` and `id="mwAw"` are now present on
both sides. The remaining difference there is the **indicator name**: service
`attrs.name` `"good-star"`, rustoid `" "` (the no-argument-transclusion bug from
the wide-corpus table, now the first thing in the way).

Validation: cache unit tests (31, incl. injective-stem, intermediate-scheme read,
long-key truncation, and the repair), `cargo test --release --workspace`,
`clippy --workspace --all-targets`, `fmt --check`, and the fixture guard
**876/896** all clean.

## `mw.ext.FlaggedRevs` was missing, and the wrapper it hid

With the cache collision fixed, COVID-19 pandemic's first difference moved to the
indicator name. The other pages the wide table blamed on the same missing
`mw-empty-elt` class did **not** move, and reading their differing window shows
why: their paragraph is not merely *missing the class*, it contains a red link.
`ISO 3166-1 alpha-2` transcludes `{{pp-pc}}`, which resolves to
`Template:Pending changes–protected` and reaches `Module:Effective protection
level`; that module's line 15 is

```lua
local level = mw.ext.FlaggedRevs.getStabilitySettings(title)
```

and rustoid exposed no `mw.ext.FlaggedRevs`, so the transclusion aborted into
`<strong class="error">Script error: … attempt to index field 'FlaggedRevs' (a
nil value)</strong>`. A rendering error is not rendering-transparent, so
`CleanUp::handleEmptyElements` declined to mark the paragraph — the same *shape*
as the cache bug, reached from a different cause.

The fix is the same kind of honest stub the neighbouring `mw.ext` entries use:
`getStabilitySettings` returns `nil`, which is how the wiki answers for a page with
no stability configuration and what `Module:Effective protection level` reads as
`level and level.autoreview` (the "no extra review requirement" branch). A table
would invent a review level the wiki does not apply.

Effect on `ISO 3166-1 alpha-2`: the Lua error is gone and the first difference
moves 600 -> 598. What remains there is now the *same* shape as Grand Theft Auto V
— a template-only line whose empty-`elt` marking lands on the inner
`<span class="mw-empty-elt">` instead of on a wrapping
`<p class="mw-empty-elt" id="mwAw">`. That is a paragraph-wrapping question, not a
cache or Lua one, and it is the next target rather than another miss.

The 9-page subset is unchanged (Sundial 1897, Zebra 2172, Megadeth 2801, List of
sovereign states 4009, Quicksilver 4213, Unix 4772, Help:Introduction 5161,
Nobel Prize 5926, Bicycle 11836), and the fixture guard stays **876/896**.

## PWrap was a misdiagnosis: the empty-`<p>` shape is protection facts

The wide table's next-shape target was read as "PWrap does not wrap the
template-only line in a `<p>`". It is not PWrap. `ISO 3166-1 alpha-2`,
`Grand Theft Auto V`, `Polio vaccine`, `Python (programming language)` and
`Chernobyl disaster` all differ at a line that *is* a template-only line, but
rustoid's transclusion output is short of the indicator that would make the line
non-transparent, so ParagraphWrapper correctly leaves it unwrapped and CleanUp
marks the inner `<span>` instead.

The line is `{{pp-pc}}` / `{{Pp-vandalism}}` / `{{pp-semi-indef}}`, which reach
`Module:Protected page` → `Module:Effective protection level` →
`mw.ext.FlaggedRevs.getStabilitySettings(title)`. With the field absent the call
was a Lua error (fixed by the stub in the previous commit). With it `nil`, the
module lands on the *unprotected* branch — "Wikipedia pages with incorrect
protection templates" — where the service shows the padlock indicator.

The reason it cannot simply be made faithful: **pending-changes protection is not
in the read API.** `action=query&prop=info&inprop=protection` on the live wiki
returns `"protection": []` for `ISO 3166-1 alpha-2` even though it *is*
pending-changes protected (re-pinning the baseline at the current revision still
shows the indicator). `mw.ext.FlaggedRevs` is a Scribunto library fed by the
FlaggedRevs extension server-side, and Parsoid core-compat gets it by running the
real Scribunto; rustoid has no equivalent source, so `getStabilitySettings`
cannot return anything but a guess. The honest stub is `nil` — it removes the
crash, and a page rustoid cannot classify reads as unprotected rather than
erroring — and these rows are recorded as blocked on data the read API does not
carry, not on a parser bug.

## `#tag` arguments, and the half that is fixed

`CoreParserFunctions::tagObj` runs the inner (second) argument and every named
attribute's **name and value** through `$frame->expand` before building the tag,
because the tag's content and attributes are strings nothing re-expands later.
rustoid expanded only the target. `expand_tag_args` now expands a `#tag` call's
named attributes the same way, and `pf_tag` trims them (`trim` first, then the
surrounding-quote strip, matching `tagObj`'s order).

That is what fixes the indicator **name**: `Template:Top icon`'s
`name = {{#if:…}}…{{{id|}}}…` was stringified unexpanded and came out `" "`;
it is now `good-star`. COVID-19 pandemic's first difference moves **962 -> 999**;
the 9-page subset improves by one (Megadeth 2801 -> 2842); the fixture guard
stays **876/896**.

The **content** half is deliberately not in this change, and the reason is worth
recording because the obvious fix is wrong. The content *is* already substituted
by the time `pf_tag` runs — the trace of
`{{#tag:indicator|[[File:{{{1|X.svg}}}|20px]]|name=foo}}` shows the wikilink's
`href` already `File:X.svg`. What is stale is the *source*: `tag_extension_token`
builds `extsrc` with `token_to_source`, which returns the token's original
`data_parsoid.src` (`[[File:{{{1|X.svg}}}|20px]]`) rather than regenerating it
from the expanded attributes. Re-running the pipeline over the content
(`frame.expand` + `expand_templates`) fixes nothing and *breaks* `#tag:pre`:
`Template:Pre` is `{{#tag:pre|{{{1}}}|format="wikitext"}}` and its `{{!}}` table
arguments are already resolved to the in-template `<td>` form, which the second
pass empties (the `Template pre: Table` fixtures fell 876 -> 874). The right fix
is source regeneration in `tag_extension_token`, not another expansion pass —
which is why the named-attribute half landed alone.

Documentation: this is the first difference on COVID-19 pandemic at byte 999
(extsrc), and on Megadeth at 2842.

### Why the content half is not a small patch

The "source regeneration" the fix above points at is a *new component*, not a
correction to `tag_extension_token`. Every token-to-wikitext path in rustoid
returns the token's original `data_parsoid.src`:
`parser_functions::token_to_source` (line 857) and
`wikitext::token_utils::tokens_to_source` (the one `lua_deferred::render_answer`
uses) both check `dp.src` first. So after an argument substitution updates a
wikilink's `href`, the only source that can be produced is still the *old* one
`[[File:{{{1|X.svg}}}|20px]]`. Reproducing core here means expanding the inner
from its *text* (core's `PPFrame` works on string nodes and regenerates) or
building a general token→wikitext serializer — rustoid is a wt2html-only port and
has neither. Recorded as the shape of the remaining work on COVID-19 pandemic's
byte-999 difference, rather than attempted as a patch.

## The `#tag` argument expansion was a wrong turn; the serializer lands alone

This section corrects the record and is worth reading before any more `#tag`
work. The document above describes `expand_tag_args` (expanding a `#tag` call's
named attributes the way `tagObj` does) as the fix that moved the indicator
**name** to `good-star`. It did that — and it was still the wrong change to land.

A wide-corpus comparison showed the output *shrinking* by about 6.7 MB
(0.72× → 0.58× of the oracle). Per page, `Israel` fell from 2,561,103 bytes to
2,074,437 and from 1091 `<li>` to 303, while the oracle has 2243 and **three**
reference lists (`fn`, `lower-alpha`, and an ungrouped list) to rustoid's two.
Rendering Israel through three checkouts isolated it:

| state | bytes | `<li>` | reference lists |
|---|---|---|---|
| before `expand_tag_args` | 2,561,103 | 1091 | 2 (`fn`, ungrouped) |
| with `expand_tag_args` | 2,074,437 | 303 | 2 (`fn`, `lower-alpha`) |
| oracle | 2,702,824 | 2243 | 3 |

The mechanism is not the expansion as such but what it feeds:
`Template:Notelist` passes `group={{safesubst:#switch:…}}` to `{{reflist}}`, and
`Template:Reflist` is
`{{#tag:references|{{{refs|}}}|group={{{group|}}}|responsive=…}}`. Expanding the
`group` **named attribute** changes which group the `<references>` handler
records, and rustoid's handler then drops the bulk of the list. **The reference
handler only tracks two groups; the oracle needs three.** That is the real bug
underneath, and until it is fixed the argument expansion cannot be turned on:
it buys a handful of bytes on a few pages and costs megabytes on the reference-
heavy ones. The named-attribute *and* the inner (content) expansion were tried;
the inner expansion additionally introduced two stalls (`India`,
`United States`). Both are reverted.

What survives the revert, deliberately:

- **the `pf_tag` named-attribute trim.** `tagObj` trims a named attribute's name
  and value before stripping a surrounding quote pair, and that normalisation is
  correct on its own — it is not an expansion, it changes nothing Israel's
  reference recording depends on, and Israel is byte-identical with and without
  it. Only the *expansion* is gone.
- **a token→wikitext serializer**, `tag_content_source` +
  `strip_html_comments`, next to `argument_value_text`. It rebuilds a `#tag`
  content's source from the tokens instead of trusting the stale
  `data_parsoid.src`: it keeps newlines (the content is wikitext, and
  `#tag:pre`'s `extsrc` carries them), drops comments the way the preprocessor
  does, rebuilds a `wikilink` from its `href`/`mw:maybeContent` **attribs** so a
  substituted target is rendered, and returns `None` for anything it cannot
  render so the caller keeps the recorded source rather than losing the
  construct. Unit-tested, and it fires on a top-level probe
  (`{{#tag:indicator|[[File:{{{1|X.svg}}}|20px]]|name=foo}}` →
  `extsrc: "[[File:X.svg|20px]]"`, the substituted target).

### The serializer is dormant on the current top differences — and the earlier
### “the content is already substituted” claim is only half true

The document above says the content is already substituted and only the source is
stale. That is true for a `#tag` written at the **top level** of a page, and it is
what the probe exercises. It is **not** true inside a template, which is where the
interesting calls live. Tracing `{{Top icon|…}}` (what `Template:Good article`
reaches on COVID-19 pandemic) shows the `#tag:indicator` content is a *single
wikilink* whose `href` **is** substituted (`File:symbol support vote.svg`) but
whose `mw:maybeContent` still holds **unexpanded** `{{#if}}` and
`{{Str number/trim}}` template tokens. `argument_value_text` refuses a live
`template` token by design, so `tag_content_source` returns `None` and the caller
falls back to the recorded source — the unexpanded `{{{image|…}}}` the page
currently shows. So the serializer is a correct, tested building block and a
prerequisite, but on today's corpus it moves **no scoreboard entry**: every
top difference is earlier (an `attrs.name` indicator, a hatnote category), and the
one case it targets is blocked by the content's own unexpanded templates.

Measurements with the serializer alone (expansion reverted, trim kept):

- fixture guard **876/896** (unchanged), `clippy --all-targets` 0 warnings,
  `cargo test --release --workspace` green;
- Israel byte-identical with and without the serializer, back to the pre-change
  baseline (2,561,103 bytes, 1091 `<li>`);
- the wide corpus is the 0.72× baseline again — same first-difference byte on
  every page (COVID-19 pandemic 962, Israel 1646, France 1601, India 1499, …).

Next step is not more `#tag` work: it is the reference handler's two-group limit,
because that is what blocks the indicator-name fix (and with it the `attrs.name`
difference that is the first byte on France, India, Canada, Periodic table and
COVID-19 pandemic).

### Correction: it is not a group limit, it is a dropped `<references>`

The sentence above repeats the earlier guess. Reproducing the regression
(re-checking out `8bf5690`'s two files, rendering Israel) says the guess is
wrong, and the real shape is simpler. Israel's reference section makes four
`<references>`-producing calls: `footnotes = {{notelist}}` in the infobox,
`{{Reflist|group=fn}}` and `{{notelist}}` under `==Notes==`, and a bare
`{{reflist}}` under `==References==`. The oracle renders **three** lists — groups
`fn`, `lower-alpha`, and the ungrouped one. rustoid renders **two**, and *which
two is what the argument expansion changes*:

| state | groups rustoid renders | missing |
|---|---|---|
| expansion off | `fn`, ungrouped | `lower-alpha` |
| expansion on | `fn`, `lower-alpha` | ungrouped |

Both of rustoid's lists sit in the Notes section (byte offsets ~1,473,686 and
~1,474,374 in a 2.6 MB render, 688 bytes apart); the References `{{reflist}}` and
the infobox `{{notelist}}` produce no list at all. `CiteState` keys groups in a
`HashMap`, so it has no two-group ceiling — the failure is that whole
`<references>` invocations are **lost or merged**, and the group value decides
which, which is why expanding `group=` reshuffles the damage instead of creating
it. That is a transclusion/identity bug (two calls colliding on something the
group feeds, an `about` id or a shared extension node), not a Cite group limit,
and it is the thing to chase next — not by re-landing the expansion, which only
moves the hole.

### Chasing the `<references>` merge: the loss is not in Cite

Following the dropped `<references>` the correction points at says it is right
in spirit and wrong in location. Rendering `Israel` and reading the byte layout,
the first page-sized defect is that the **Etymology paragraph's
`mw:Transclusion` wrapper carries a ~416 KB `data-mw`** whose `parts` absorb the
*rest of the page* as one string:

    <p about="#mwt27" typeof="mw:Transclusion"
       data-mw='{"parts":[{"template":{"target":{"wt":"Further",…}}},
                        "\nThe names [[Land of Israel]] … ",
                        {"template":{…"lang"…}}, …,
                        ", \"[[El (deity)|El (God)]] persists/rules\") refers to
                        the patriarch [[Jacob]] … ===Sports=== …
                        ==Notes== … ==References== … {{reflist}} … }]}'>

The trapped text is a *JSON string*, so everything after the Etymology paragraph
— including `==References==` and its `{{reflist}}` — is never parsed as a
section. That is why `id="References"` is absent from rustoid's Israel and why
the ungrouped `<references>` list never renders: the reference section is there,
but as text inside an attribute. The same wikitext also occurs again later in the
document (`Pawn stars` appears three times), so the page is duplicated as well.
So this is not a Cite group limit and not a `<references>` merge: it is the
transclusion encapsulation window, upstream of `ext/cite.rs`.

Minimal reproduction, six lines, no Israel required:

    ==Etymology==
    {{Further|Israel (name)|Names of the Levant#Israel and Judea}}

    The names [[Land of Israel]] {{langx|grc|X}}

    ==SENTINEL==
    X

`X`'s paragraph comes out as
`data-mw='{"parts":[{…"Langx"…},"\n\n==SENTINEL==\nX"]}'` — the sentinel is
both trapped in `parts` *and* rendered. Every ingredient is needed: removing the
heading, renaming it `==Foo==`, dropping the second `Further` parameter, using a
plain template instead of the hatnote, replacing the wikilink, or changing the
language code `grc` to `xx` each makes the trap go away. That the *heading text*
and the *language code* matter is the tell that it is offset/identity-sensitive
rather than a regex misfire — and it cannot be the trail merge, because the
default `link_trail_regex` is `^[a-z]+` and cannot match `"\n\n==SENTINEL==\nX"`.
The shape points at the window end computed when a template whose expansion is a
block (`{{langx|grc|…}}` → `<dl>`) closes a paragraph under a heading.

Not yet pinned: where that window end is computed, and whether the trap and the
duplicate are one bug or two. That — not `CiteState` — is the next target: it is
what actually breaks Israel's reference section.

## The window end: a first (partial) fix, and the bug it uncovers

The window end from the previous section is in `wrap_flipped_children`
(`tree_builder_html.rs`). It computed `range_end` as the **max `dsr.end` over
*every* following sibling**; PHP's `encapsulateTemplates` uses
`getRangeEndDSR($range)` — the DSR of the *range's end node* — and only that
(`$dp1DSR->end = $dp2DSR->end`). The over-broad max is what dragged the rest of
the page in: on the six-line reproduction it took the `==SENTINEL==` heading's
DSR, and on Israel the `==References==` section's, straight through EOF.

Two things were wrong, not one. Restricted to the end-marker sibling
(`children[t]`) plus any sibling the adoption pass stamped with the range's
`about`, the value was still bad, because the offsets came from two coordinate
systems. Tracing the reproduction's sibling list shows the end-marker `<meta>`
at `dsr = (131, 131)` — the template *output* length — while its `tsr` is
`(null, 122)`, the *source* end of `{{langx|grc|X}}`, and the sentinel nodes are
source-relative. Slicing `source[122 .. 131]` yields `"\n\n==SENTI"`. The fix
reads the source offset from `tsr.end` (falling back to `dsr.end`), which is what
a template marker's `tsr` is for.

Effect: the reproduction renders
`data-mw='{"parts":[{…"Langx"…}]}'` with no trailing part, and Israel loses
about **900 KB of bogus `data-mw` echo** — a 416 KB attribute on the Etymology
`<p>` was the first. Nothing rendered changed: `<p` 141, `<li` 1091, `<a` 3033,
`<td` 90 are **identical** before and after, the wide corpus keeps the same
first-difference byte on every page, and the fixture guard stays **876/896**.
So the earlier "closeness" to the oracle's byte count was padding from the bogus
echo, not content.

### It does not yet fix Israel's references

A second, different bug remains, and it is why `id="References"` is still
absent (`mw-references-wrap` 2 where the oracle has 3). A `<ul about="#mwt468">`
wrapper on Israel carries `parts` sliced from the **top of the page**:

    {"parts":[{"template":{"target":{"wt":"PAGENAME","function":"pagename"},…}},
               "ription|Country in West Asia}}\n{{About|the country|the region|…",
               {"template":…"Template:2"…},
               "ob|other uses}}\n{{pp-extended|small=yes}}\n{{Use…"]}

`"ription|Country in West Asia}}"` is the tail of `{{Short description|Country in
West Asia}}`, cut mid-word. This is the *other* half of the same PHP pair:
`recordTemplateInfo` interpolates the wikitext *between* templates
(`$prevTplInfo->dsr->end … $dsr->start`) and the leading `unwrappedWT`, and those
gaps are being sliced with output-coordinate offsets exactly like the end marker's
`dsr` was. rustoid's `build_compound_data_mw_with_nested` and
`attribute_expander::split_tokens` are where to look next. Until that is fixed the
References section stays trapped, so this remains the target — but it is now two
known, reproducible defects rather than one unlocated one.

### Locating the compound-parts bug: it is the grouping, not the slice

Instrumenting the parts construction narrows the second defect to one line of
output. `compute_range_plan` splices the wikitext between two constituents of a
compound transclusion as
`source[prev_end .. r.start_offset]` with `prev_end = ranges[c].dsr_end`, and for
the `Authority control` `<ul>` (top-level range `#mwt468`) it produces:

    BIG_GAP top=#mwt468 c=#mwt463 len=446141 pe=130 start=446271

446 KB spliced from offset 130 to 446271 — the same page tail that traps the
References section. So `#mwt468` is being treated as a *compound* whose
constituents span from near the top of the page to the Sources section; the huge
part is a symptom of that grouping, and the gap slice is faithful given it.

Four candidate causes were ruled out by measurement (each changed nothing on
Israel and kept the fixture guard at 876/896):

- `RangePlan::compound_data_mw`'s trailing tail,
- `build_compound_data_mw`'s leading `unwrappedWT` and trailing parts,
- `handle_link_neighbours::migrate_parts_json` (no text node > 1000 bytes is ever
  migrated),
- a `tsr`-vs-`dsr` preference in `collect_plan_metas` (the two agree here).

That leaves the *grouping*: which ranges become constituents of one compound —
`compute_range_plan`'s `subsumed` graph, `top_level_enclosing`, and the
overlap-merge test `in_document_order(range_start, prev.range_end)` — a port of
`DOMRangeBuilder::findTopLevelNonOverlappingRanges`. That is the next target, and
it is a correctness fix to the range graph rather than to any string slice.

### The range graph against PHP: four divergences, and the one I tried

Checked line by line against `DOMRangeBuilder.php` (v0.24.0-a23,
`src/Wt2Html/DOM/Processors/DOMRangeBuilder.php`). Four places where rustoid's
`compute_range_plan` does not follow PHP:

1. **Start-marker gate.** PHP takes a start marker as a range start only when
   `!empty(getDataParsoid($elem)->tsr)` (`findWrappableTemplateRangesRecursive`);
   an end marker is always taken. rustoid gates the start on `dsr_start.is_some()`,
   which admits template-content markers whose `dsr.start` is a bogus 0 / 94.
2. **`startOffset`.** PHP: `DOMDataUtils::getDataParsoid($startMeta)->tsr->start`
   (`findEnclosingRange`). rustoid: the marker's `dsr.start`.
3. **Flipped ranges.** PHP's `addNodeRange` walks
   `!flipped ? start : end` … `!flipped ? end : start`, and `rangesOverlap` swaps
   the ends for a flipped range. rustoid has no `flipped` notion in either.
4. **Order.** PHP's `DOMUtils::inSiblingOrder` compares DSR starts; rustoid's
   `in_document_order` compares tree paths (`a <= b`).

Aligning (1) alone — gating the start on the marker's `tsr` — did what the graph
analysis predicted: it removed the 446,141-byte `BIG_GAP` and the whole `#mwt468`
compound build, and left fixtures at **876/896**, `clippy` and `fmt` clean. But it
moved **no** first-difference byte on the 41-page corpus while changing output
*sizes* on four pages, in **both** directions (33881→33424, 17881→17880,
1348594→1348681, 2168375→2168667). A change with no scoreboard effect and
inconsistent metadata deltas is not demonstrably an improvement, so it was
reverted: (1)–(4) are coupled and want to be done as one change, measured on the
corpus.

### The trap is not in the range plan at all

The `<ul about="#mwt468">`'s ~460 KB `data-mw` is never built by any path I
instrumented. Logs that fire on a `data-mw` over 20 KB were added to
`build_compound_data_mw`, `build_compound_data_mw_with_nested`,
`merge_encap_data_mw`, `RangePlan::compound_data_mw`, and the `resolve_data_ids`
stash — zero of them fired on Israel (only the legitimate 21 KB `Refbegin`), yet
the attribute is there, fully assembled. Its `parts` (`PAGENAME`, `Template:2`,
then the page tail as literal text) come with the transclusion from the
template/Lua layer, before any DOM encapsulation runs.

That layer is also where `safesubst` leaks: it appears **14** times in rustoid's
Israel and **0** in the oracle, the first at byte 301,434 — *before* the trapped
region — e.g. the five-brace `{{{{{|safesubst:}}}#if:1473946…` seen in the
`Authority control` output. An unbalanced brace run is exactly what would make the
rest of the page read as literal text inside the transclusion. So the next thread
is the `safesubst` / `{{ … |safesubst:}}` handling in the template layer, not
`compute_range_plan`.

### The safesubst thread: the brace run, and the modifier (fixed)

Following that lead found two real bugs, both now fixed (`3bd3528`).

**1. The brace run.** `Template:Convert` is
`{{{{{♥|safesubst:}}}#invoke:convert|convert}}`. MediaWiki's grammar gives the
rule explicitly (`Grammar.pegphp`, `tplarg_or_template`, "ideal precedence"): a
run of N `{` opens a template when `N % 3 == 2`, an argument when `N % 3 == 0`,
and when `N % 3 == 1` one `{` is literal text and the rest is reconsidered — so
five braces are `{{` + `{{{` and six are `{{{` + `{{{`. rustoid's
`parse_directive` chose on `starts_with("{{{")` alone, so the five-brace form was
read as an argument and leaked. Minimal repro, no templates needed:
`{{{{{x|#if:}}}1|yes|no}}` gave `{#if:1|yes|no}}` and now gives `yes`. The tell
was that the *same* construct with a space after `{{` (`{{ {{{x|#if:}}}1|yes|no}}`)
always worked, which isolates the fault to how the run of braces is split.
`parse_directive` now chooses on the run length mod 3 and `parse_template_token`
no longer bails on `{{{`. Effect on Israel: `safesubst` occurrences **14 → 0**.

**2. `safesubst` as a modifier on a *dynamic* name.** With (1) fixed,
`{{Convert|750|m|}}` stopped leaking but reported "Template loop detected:
Template:Convert": the name only becomes `safesubst:#invoke:convert` once the
argument reference expands, and `resolve_target_string` computed `has_hash`
*before* stripping the `safesubst:` modifier, so `#invoke` was never recognised
as a parser function and the whole name was resolved as a template title.
Computing `has_hash` after the strip fixes it: `{{Convert|750|m|}}` now renders
`750 metres (2,460 ft)`.

Validation: fixtures **876/896**, `clippy`/`fmt` clean, no new stalls. On the
41-page corpus the first-difference byte is unchanged — every page still differs
earlier, at the hatnote category or the indicator name — while 22 pages grow as
the previously-leaked templates expand. This does not touch the References trap
(still 2 of 3 lists, `id="References"` absent, whose `data-mw` is assembled
before the DOM layer), but the leaked text this section suspected of unbalancing
the preprocessor is gone.

## The References trap: two bugs, and the `#mwt1666` tsr that ties them together

The trap is solved, and it was two independent defects that produced the same
symptom. The thread that finally located the first one is worth recording because
it is a template for the next one: **instrument the boundary, do not reason from
the category label.**

### Locating it: the marker with a page-relative `tsr` it had no right to

Logging every `data-mw` over 20 KB inside the DOM encapsulation
(`transfer_transclusion_to_element`, `RangePlan::compound_data_mw`) showed the
alleged "assembled before the DOM layer" 457 KB attribute *is* built by the range
plan, for the `External links` `<ul>` `about="#mwt468"`. The plan's own log named
the culprit:

    TRAP_PLAN_GROUP top=#mwt468 n=7 first=#mwt1666 (off 0) last=#mwt468 (off 446972)

A range `#mwt1666` with `start_offset = 0` was being absorbed into `#mwt468`, so
the gap slice `source[0 .. 446271]` — the whole page tail, References section
included — became one `parts` string. `#mwt1666` is a `{{PAGENAME}}` marker
inside a wikilink target: `Template:Wikiatlas` is
`[[commons:Atlas of {{PAGENAME}}|Wikimedia Atlas of {{PAGENAME}}]]`, and the
wikilink's `href` is attribute-expanded at page level. Its `tsr` was `(0, 12)`
with `source: None` — i.e. **relative to the sub-source `{{PAGENAME}}`
(12 characters), presented as a page offset.** `page_source[0..12]` is
`{{Short desc`, so the marker looked like a top-level transclusion starting at
offset 0.

### Fix 1: clear `tsr` on template content, as PHP does

PHP clears the `tsr` of everything a template / parser function / variable
expansion produced (`TemplateHandler::processTemplateTokens`:
`unset( $t->dataParsoid->tsr )`), and `DOMRangeBuilder` relies on it —
`findWrappableTemplateRangesRecursive` only takes a start marker as a range start
when the `tsr` is set, precisely *because* template content has none. rustoid
applied that clearing only to parser-function output (`parser_functions_wrapper`),
so a variable expanded inside an **attribute value** kept a sub-source `tsr`.
`Parser::expand_attributes` now runs `token_utils::clear_tsr` over each attribute
key/value expansion, mirroring `processTemplateTokens`. Israel's `#mwt468`
`data-mw` drops from 457 KB to the legitimate 1.3 KB (the `official website`
list), and the `==References==` section is no longer trapped as text.

That alone did **not** make `id="References"` appear, though — because of the
second bug.

### Fix 2: the section wrapper dropped a section it had nested

With the trap gone, the unwrapped render had **3** reference lists and the
References heading, but the wrapped render (what the wiki serves, and what the
oracle has) still lost the whole section, and section id **48** was simply
absent. `RUSTOID_TRAP_SEC` logging in `section_wrapper::wrap_level` showed the
References heading *was* seen and wrapped. The bug: opening a section onto the
nesting stack and later **popping** it discarded it —
`while … { stack.pop(); }` in the heading branch threw the node away, and only
the single `current` section was ever committed. On Israel:

    Notes (h2)      → current
    References (h2) → Notes committed; References = current
    Sources (h3)    → References pushed onto the stack
    External links (h2) → References popped and DISCARDED

So the `==References==` section, with `{{Duplicated citations}}` and
`{{reflist}}`, vanished — a bug that also cost a section on every page with a
nested-then-popped heading (`Doom`, `Isaac Newton`, `Association football`,
`2024 Summer Olympics`, …). `wrap_level` is rewritten around a single stack
whose top is the innermost open section; `close_above` attaches each closed
section to its parent (or emits it as a top-level sibling) instead of dropping
it, and the lead section is kept out of the stack so nesting cannot swallow it.
A regression test (`test_section_nesting_a_deeper_heading_survives_a_later_sibling`)
pins the exact shape.

### Result and validation

Israel, wrapped, against the pinned oracle:

| metric | before | after | oracle |
|---|---|---|---|
| `mw-references-wrap` lists | 2 | **3** | 3 |
| `id="References"` | absent | **present** | present |
| `<section data-mw-section-id>` | 42 | **51** | 51 |
| `<li>` | 1091 | 1940 | 2243 |
| bytes | 1 196 123 | 1 833 465 | 2 703 684 |

Fixtures stay **876/896**, `clippy --all-targets` 0 warnings, `cargo fmt
--all --check` clean, `cargo test --release --workspace` green.

On the 41-page corpus the **first-difference byte does not move on any page** —
every page still differs earlier, at the hatnote category or the indicator name —
and the score is still **0/41**, so this buys no scoreboard entry yet. What it
does buy is honest: total rustoid output rises from 0.61× to **0.65×** of the
oracle, four pages shrink because their traps are removed (not because content is
lost), and on the four biggest shrinkers the `<section>` and `mw-references-wrap`
counts now **match the oracle exactly** (`Doom` 23/2, `Association football`
30/2, `Isaac Newton` 41/2, `2024 Summer Olympics` 42/3-of-4). The remaining
`<li>` shortfall is the reference *content* gap, not section structure.

### Re-landing the `#tag` named-attribute expansion, with the record corrected

With both bugs above fixed, the `#tag` named-attribute expansion that
`908f689` reverted (`8bf5690`) was re-landed. Its revert recorded the reason as
"the reference handler only tracks two groups … the expanded group makes it drop
the bulk of the list" — and that record is **wrong**, so it is corrected here.
What actually happened:

- Without the expansion, `Template:Reflist`'s `group={{{group|}}}` stayed
  unexpanded, so every `<references>` call recorded an **empty** group. Israel's
  bare `{{reflist}}` and `{{notelist}}` *both* rendered the ungrouped list, and
  the page carried **748 duplicated `cite_note` ids** — invalid HTML, the same
  notes twice.
- With the expansion, the groups are `fn`, `lower-alpha` and ungrouped, written
  separately, and every `cite_note` id is unique: **748 distinct notes, oracle
  765.**

The "drop" from 1496 to 748 ids is the duplicate going away, not content being
lost — the earlier reading compared the wrong number. The real reference loss
that *did* exist (Israel's missing `==References==`) was the trapped section and
the section-wrapper drop, both fixed above; the expansion was blamed for a
different bug's symptom.

Effect: the corpus first-difference byte moves later on three pages and never
earlier (`Doom` 896→937, `Megadeth` 2801→2842, `COVID-19 pandemic` 962→999);
total output falls from 0.65× to 0.56× of the oracle, which is the duplicate
removal. Fixtures **876/896**, `clippy`/`fmt` clean, workspace tests green.
The residual gap — 748 notes where the oracle has 765, and the `fn` /
`lower-alpha` lists empty where the oracle has 6 and 9 — is *collection*, not
structure, and is the next thing to chase.

### The collection gap was a brace counter in `find_arg_separator_eq`

Chasing that gap reached the tokenizer. A minimal reproduction isolates it
without Cite:

    {{#if:1|{{t|{{{a|{{{b|C}}}}}}|g=f}}|X}}   →   {{t|{{{a|{{{b|C}}}}}}|g=f}}

The `#if` branch is returned as **literal text**, and instrumenting `pf_if`
showed why: its arguments were split as `1`, `{{t|{{{a|{{{b|C}}}}}}|g`, `f}}`,
`X`. The branch's `=` in `|g=f` was taken for a `name=value` separator, so the
whole branch became one named argument.

`find_arg_separator_eq` only splits on a **top-level** `=` (PHP's
`template_param_name`), and it tracked nested braces with a flat counter: `+1`
per `{{`, `−1` per `}}`. But `{{{` is three braces and its closer `}}}` is
three braces, so a run of six `}` — two tplarg closers — decremented the
counter **three** times. On `{{t|{{{a|{{{b|C}}}}}}|g=f}}` that drove the depth to
0 before `|g=f`, and the nested `=` looked top-level. The argument *splitter*
(`split_template_args_impl_offsets`) already used a stack of innermost closers
and got this right; `find_arg_separator_eq` now uses the same stack.

Why it mattered so much: `Template:Refn` and `Template:Efn` both build their
footnote as `{{#tag:ref|…|group=…}}` **inside an `#if`** — exactly the shape
here — so every grouped footnote was emitted as raw wikitext and the `fn` /
`lower-alpha` lists stayed empty. That is the "collection" gap: the refs were
never collected because they were never built. With the fix Israel's groups
render (755 notes, oracle 765; `fn` and `lower-alpha` present).

Effect: corpus output grows on 30 of 41 pages and shrinks on none of
consequence (total +257 KB toward the oracle: `List of sovereign states`
+79 KB, `India` +37 KB, `Periodic table` +19 KB), while **no first-difference
byte moves** — the remaining diffs are still the early hatnote category and
indicator name. Fixtures **876/896**, `clippy`/`fmt` clean, workspace tests
green.

### The indicator body: `tagObj` expands the inner content too

The remaining first difference on the protection/indicator pages was the
`<indicator>` body. `Template:Top icon` is

    {{#tag:indicator|[[File:{{{image|{{{imagename|{{{1|}}}}}}}}}|{{#if:...}}…]]|name=…}}

and rustoid recorded its `extsrc` as that literal text — unexpanded — where the
oracle has `[[File:symbol support vote.svg|20x20px |link=Wikipedia:Good
articles*   |This is a good article. Click here for more information.]]`. Core's
`tagObj` runs the inner (second) argument through `$frame->expand` as well as
the named attributes, because the content is a string nothing re-expands later;
`expand_tag_args` expanded only the named attributes (the inner half had been
left open deliberately — see the `#tag` revert above). It now expands the
positional content, and the indicator body matches the oracle byte for byte.

One exception, and it is the one that matters for speed: a `#tag:ref`'s content
is the reference *body*, which the Cite extension renders from the recorded
source. The served `<ref>` records only `body.id`, never an `extsrc`, so
nothing observable changes by skipping it — and expanding it made `India`'s
render take minutes, because the body's templates were expanded a second time.
With `ref` skipped, `India` completes again and the rest of the corpus is
unchanged. (A first attempt that expanded *every* content including `ref`
reproduced the documented `India`/`United States` stall; the trace showed a
single `#tag:ref` content expansion that never finished.)

Effect: `Doom (1993 video game)` first difference 937 → **5543**, `Megadeth`
2842 → **7463**, `COVID-19 pandemic` 999 → **1238**; no page moves earlier, the
other 38 are unchanged, and total output is flat. Fixtures **876/896**,
`clippy`/`fmt` clean, workspace tests green.

The next stop on `COVID-19 pandemic` is 1238: the empty-transclusion shape — the
oracle serves `<p class="mw-empty-elt" id="mwAw"><span typeof="mw:Nowiki
mw:Transclusion" about="#mwt4">…</span><meta typeof="mw:Extension/indicator"
…/></p>` where rustoid emits the `<span class="mw-empty-elt" …>` without the
`<p>` wrapper or the `mw:Nowiki` typeof.

### Correction: that was not the empty-transclusion shape, it was missing facts

The paragraph above is wrong, and the way it was wrong is worth keeping. The
`<p class="mw-empty-elt">` grouping was **already correct**; the corpus diff at
1238 was `COVID-19 pandemic`'s *protection* template, whose output differs when
the page reads "unprotected". Chasing it reached three separate cache/fact gaps,
not a PWrap bug:

- **Protection.** `Module:Protection banner` reads
  `mw.title.protectionLevels`; uncached, every article page read "unprotected"
  and the module emitted its "incorrect protection template" category where the
  service emits the padlock. The cache held protection for templates and
  subpages but not for article pages. `populate_taxonomy` gains a `protection`
  mode (one `prop=info&inprop=protection` request per batch). Effect on the
  first difference: `Chernobyl disaster` 588 → **5795**, `Grand Theft Auto V`
  546 → **3895**, `COVID-19 pandemic` 1238 → **10018**.
- **`Template:Cs1 config` was absent from the cache**, so `{{Cs1 config}}`
  rendered as a red link *inside* the lead paragraph — which made the paragraph
  non-empty and cost it its `mw-empty-elt` class, the actual diff on `Polio
  vaccine`. Fetching it moved Polio 572 → **5306**.
- **A sitelink entry that existed on disk but not in the manifest.** `Zebro`'s
  `entity:sitelink:Zebro` (→ `Q51881083`) was written as a body but never
  indexed, so `mw.wikibase.getDescription(nil)` saw no current entity and
  `Module:SDcat` answered `Short description with empty Wikidata description`
  where the oracle says `…is different from Wikidata` (the entity's description
  is `"unidentifed wild or feral equine"` — the local one is `"Unidentified…"`,
  so they genuinely differ). `rustoid-compare --wiki www.wikidata.org --reindex`
  recovered 14 bodies; Zebro's first difference moved 544 → **3026**.

All four are **facts a page body cannot supply**, which is why an offline render
cannot infer them and why the fixes are cache population rather than code.

### Newly exposed: `title.categories`

Caching protection turned five pages `extendedconfirmed`, and on those
`Module:Protected page` (line 881) now calls
`inArray(protectionObj.title.talkPageTitle.categories, …)` — and rustoid's Lua
`mw.title` has **no `categories` field at all**, so the call errors. On the wiki
the talk page always has categories (WikiProject banners), so the field is a
table; supplying it needs the talk page's body, which rustoid never fetches.
That is the next real gap those pages will hit, after the protection category
is correct.

Updated scoreboard head (wide corpus, offline): `Module:Math` 1, `Python`
578, `ISO 3166-1 alpha-2` 598, `World War II` 939, `Hydrogen` 976,
`Association football` 1438. Score still **0/41**.

### Link-resolution facts: the `mw-disambig` class and the hatnote category

The same class of gap, one layer down. `AddRedLinks` asks about *every* wikilink
on a page; uncached, an offline run cannot say whether a target exists or is a
disambiguation page, so a `… (disambiguation)` link lost its `mw-disambig` class
and a hatnote's existence check answered "nonexistent" — the
`Category:Articles with hatnote templates targeting a nonexistent page` that the
oracle does not have. `populate_taxonomy` gains a `pageinfo` mode (one
`prop=info|pageprops` request per batch of titles). Nine pages' first difference
moved later by ~50 bytes each (`Anarchism` 1571 → 1622, `Hydrogen` 976 → 1026,
`Chess` 1649 → 1696, `France` 1601 → 1649, `India` 1499 → 1546, …), none
earlier.

That test also *falsified* the earlier reading of the scoreboard: `Association
football` did **not** move. The reading that replaced it — "its remaining diff is
a missing `id` on the hatnote `<div about="#mwt2">`, a node-id assignment
question, not a link fact" — was also wrong, and in a way worth its own section:
the missing id *was* a link fact, one removal away (see "The hatnote `id` was a
*consequence* of the missing fact" below). The useful part of the mistake is that
the page did not move on this test while the id question looked like a parser
gap, which is the shape of a fact that is queried through a different door than
the one the test filled.

### The pattern, stated once

Every one of this round's wins came from a **fact a page body cannot supply** —
page protection, a link target's existence/class, the page's own Wikidata entity
(and its sitelink manifest entry) — not from the parser. The parser is now close
enough that these gaps are what the smallest first differences are made of. The
durable lesson for this work is recorded above and repeated here because it cost
an hour to learn: *a difference that looks like a PWrap or empty-element shape
may be a template rendering differently because a fact is missing.* Read the
bytes the two sides actually produce; do not infer the pass from the shape.

## The hatnote `id`, the `%S` complement, and the substituted argument

Three fixes, in the order the evidence forced them. All three were found by
reading the differing bytes on `Association football`, whose first difference
went **1438 → 9366**; `Hydrogen` moved 1026 → 3722 and `Megadeth` 7463 → 7766,
and no page's first difference regressed. Fixtures stay **876/896**.

### The hatnote `id` was a *consequence* of the missing fact, not an id bug

The recorded reading of this difference was wrong and is corrected here. The
scoreboard said the hatnote `<div about="#mwt2">` was missing `id="mwBA"`,
and that read like a node-id assignment bug (`pagebundle::assign_node_ids`).
It was not. The missing id and a spurious tracking category were the *same*
bug seen twice:

```
<span class="mw-empty-elt" about="#mwt2" typeof="mw:Transclusion" … id="mwAw"><style …/></span>
<div role="note" class="hatnote navigation-not-searchable" about="#mwt2">"Soccer" redirects here…</div>
<span class="mw-empty-elt" about="#mwt2"><link rel="mw:PageProp/Category" href="./Category:Missing_redirects"/></span>
<p class="mw-empty-elt" id="mwBA">…
```

`{{Redirect|Soccer|other uses|Soccer (disambiguation)}}` runs
`Module:Redirect hatnote`, which asks `mw.title.new(v).exists` for the redirect
term. rustoid answered **false** for `Soccer` — a page it had no body for, though
the cache held `info:Soccer` — so the module emitted `Category:Missing redirects`
and `Module:Hatnote` emitted the nonexistent-page category. Those two `<link>`s
became a *trailing* `<span>` in the `#mwt2` range, which made the hatnote `div`
**interior** rather than the range's `last` node — and `CleanUp::markDiscardableDataParsoid`
explicitly exempts the range's last node from the empty-`DataParsoid` treatment
that gives it the slot an id is allocated from. Remove the phantom categories and
the div becomes `last` again and takes `id="mwBA"`.

So the fix is a fact, not a parser pass. `mw.title`'s `exists`/`isRedirect` were
derived from `get_page_content`, which for an offline run is `None` for both
"absent" and "not cached"; they now consult the recorded page-info as well.

That needed a second entry point on `DataSource` rather than a change to
`get_page_info`: the link-resolution call deliberately answers **"exists"** for a
title it cannot check, so an offline run paints nothing red — a good guess for a
link and a wrong one for a module, because a wrong *yes* makes it load a page it
has not got (the reason `preload_titles` narrows to a namespace in the first
place). `get_known_page_info` therefore answers only what is *known*: an uncached
title is simply absent from the map, and the caller falls back to the body. Both
share one lookup in the harness, differing only in that fallback.

### `%S` never matched: the pattern engine had no complement classes

With the hatnote fixed, `Association football`'s next difference was an infobox
row whose `{{Plainlist|…}}` rendered **empty**. It reduced to
`{{Infobox sport|name=Test|nickname={{hlist|a}}}}` — any template-valued
infobox parameter did it — and the module reported
`Category:Articles using infobox templates with no data rows`: `Module:List`'s
`frame.args` was empty.

`Module:List`'s argument filter is
`mw.ustring.find(value, '%S')` to drop blank values, so an empty list means that
call answered "not found" for `a`. It did: `LuaClass::from_letter` maps only the
*ten lowercase* letters, and `single_match`/`match_bracket_class` fell through to
`class_char == subject_char` for anything else — so `%S` compared the letter `S`
against the subject and never matched. Lua's rule is `match_class`:
`tolower(cl)` selects the class and `isupper(cl)` negates it. `LuaClass::class_matches`
now does exactly that, and both call sites use it. `%B` (a non-class letter) is
still a literal escape, which the new test pins.

This is the "pattern engine" item from the todo list, and it was load-bearing
rather than cosmetic: it emptied every list built by `Module:List`, which is
`{{hlist}}`, `{{plainlist}}`, `{{unbulleted list}}` and the rest.

### The module argument gets its *substituted* text, not its source range

That fixed `{{hlist}}` everywhere except inside a template's argument. The
remaining shape is `Template:Infobox sport`:

```
| data3 = {{{nicknames|{{{nickname|}}}}}}
```

with `{{Infobox|…|data3={{{nicknames|{{{nickname|}}}}}}|…}}` passed down to
`{{#invoke:Infobox|infobox}}`. The module reads `frame:getParent().args.data3`,
and rustoid handed it `{{{nicknames|{{{nickname|}}}}}}` — the *recorded source
range* — where the service hands it the value's text. Re-expanded in the
*module's* frame, `nickname` does not exist (it is an argument of `Infobox
sport`), so the reference collapsed to its empty default and the row rendered
blank.

The instrumented argument showed why the two disagree. The tokenizer already
substituted the reference when it tokenized `Template:Infobox sport`'s body, so
the value's *tokens* are `{{Plainlist|…}}` while its `src_offsets` still span the
unsubstituted `{{{…}}}`:

```
data3 toks = [Str(" "), Tok(template) src=Some("{{Plainlist|*a}}"), Tok(nl)]
```

`argument_value_or_source` renders expanded tokens when it can and falls back to
that source range when it cannot — and an expansion that produces markup cannot
be rendered, so the stale range was what reached the module. The renderer now
runs *before* the templates are expanded (`argument_value_text` gained an arm
that renders a `template`/`template3` token from its own `src`), and its result
is recorded on the value as `vsrc`, which `kv_value_source` prefers. The value's
source is then the text the module must see, and re-expanding **it** in the
module's output is what puts the real markup back.

### Two of those three were ordinary bugs, and closing them helped widely

The wrapper and the `body` field turned out to be a node the pass never reached
and a field written unconditionally; only the id numbering is the design
difference. Both were found by instrumenting `stash_stashable_runs` (PHP's
`handleFirstRenderingTransparentNode`) with a candidate dump, which showed that
for a `<style>` inside a table cell the pass produced **no candidate at all**.

**The walk stopped at `tbody`.** `DOMRangeBuilder`'s stashing pass walks the
range and, for each `isRemexBlockNode`, runs a traverser over its whole subtree.
rustoid recursed into the *wikitext* block set, which has no `tbody`/`thead`/
`tfoot` — those are the HTML tree builder's elements, not wikitext tags — so the
walk ended at `tbody` and never reached a cell. `is_remex_block_node` is now
ported from `DOMUtils` (an element that is neither inline-only nor metadata).

That fix is broad, because it is exactly the infobox shape: `Association
football` 9366 → **9496**, `Chess` 1696 → **5449**, `Anarchism` 1622 → **2535**,
`Isaac Newton` 1546 → **4073**, `Periodic table` 1754 → **4265**, `France` 1649 →
**19849**, `Nigeria` 1564 → **2187**, with no page's first difference regressing.

**`data-mw`'s `body` was written unconditionally.** The served bytes keep the two
source forms of `<templatestyles>` apart, and the live transform endpoint settles
it: `<templatestyles src="Plainlist/styles.css"/>` serves
`{"name":"templatestyles","attrs":{…}}`, while
`<templatestyles src="Plainlist/styles.css"></templatestyles>` serves
`…,"body":{"extsrc":""}}`. The pair is what
`frame:extensionTag{name='templatestyles', …}` produces, which is why
`Module:Hatnote`'s stylesheet has a body and `Template:Plain list`'s does not.
The tokenizer already records the distinction — `extTagOffsets` is set only for a
matched end tag — so `style_node` now takes it. The unit test that asserted the
always-body shape had encoded the bug and now pins the self-closing form, with a
second test for the pair.

Neither of these moved a first difference on its own (the `body` field sits
behind the `about` id that still differs, and the wrapper fix moved later pages),
which is the reason both are recorded rather than left as a byte-level footnote.

### What is left on this page, and what it says about the design

At byte 9496 the only remaining difference in that value is the id itself:
`about="#mwt10"` on the service against `about="#mwt12"` in rustoid. That is the
design difference this round did not close. The service hands a module the
argument **expanded** — the probe `{{Infobox|data2={{Plainlist|*a}}}}` against
the live transform endpoint shows the `<div class="plainlist ">` with *no*
`about`/`typeof`, i.e. literal HTML in the module's output. rustoid hands it the
unexpanded wikitext, so re-expanding it in the module's output builds a fresh
transclusion with its own `about` id — the two extra ids. Closing this needs the
expanded value serialized back to text (the value's own HTML/wikitext), which is
the token-to-wikitext serializer already on the list; the `argument_value_text`
whitelist is the other half of the same work.

One cost is worth recording because it is easy to mistake for a bug: the fix
expands values that previously fell back to a source that re-expanded to
*nothing*, so `India` grew from under 180 s to **3 m 0 s**. Its first difference
still moved *later* (1499 → 1557), and the 180 s stall cap in the corpus recipe
therefore reports it as stalled (at a 260 s cap it completes, and `United
States` is the only stall left, as before). That is real work being done, not a
loop.

### Scoreboard

Wide corpus (48 titles, offline, 180 s cap unless noted): first-difference byte
below is the best of this round against the previous `/tmp/wide_pi.txt` run.

```
Module:Math                 1        (1)        Anarchism            1622 -> 2535
Python (programming)        578      (578)      Isaac Newton         1546 -> 4073
ISO 3166-1 alpha-2          598      (598)      Periodic table       1754 -> 4265
World War II                939      (939)      Hydrogen             1026 -> 3722
Association football        1438 ->  9496       Megadeth             7463 -> 7766
Zebro                       3026     (3026)     Nigeria              1564 -> 2187
Chernobyl disaster          5795     (5795)     France               1649 -> 19849
Grand Theft Auto V          3895     (3895)     Chess                1696 -> 5449
Doom (1993 video game)      5543     (5543)     India                1546 -> 1557
Polio vaccine               5306     (5306)     COVID-19 pandemic   10018    (10018)
```

No page's first difference regressed, and the score is still **0/41** — the
smallest remaining differences are `Module:Math` at byte 1 (a namespace/content
model gap of its own), then four pages that all differ inside the first paragraph
or hatnote. Total output rose 28.10 M → **28.26 M** bytes (0.56× → 0.57× the
oracle), the growth being content that previously re-expanded to nothing.

## The frame a template's arguments are expanded in

`Polio vaccine` differed at 5350 on a **spurious error**: rustoid served
`Template loop detected: Template:Main other` where the service renders the
category links. It reduces to one line against the live endpoint:

```
{{Main other|{{Main other|X}}}}     service: X      rustoid: Template loop detected
```

`Template:Main other` does not call itself, so no loop exists. The chain rustoid
checked was, at the point the inner call was expanded:

```
Template:Main other (whose body is being expanded)
  Template:Short description          <- the value being spliced in
    Template:Infobox drug
      Polio vaccine
```

`Template:Short description` ends with `{{Main other|{{SDcat|…}}}}`, and
`Template:Infobox drug` wraps its own short-description call in
`{{Main other|…}}` — so *whether the inner call runs before or after the outer
frame is entered* decides whether this looks like a loop. It is not one, because
the service expands a call's argument values **in the calling frame**, before the
callee's frame exists. rustoid spliced them into the body and expanded them
there, i.e. in the callee's frame — one frame too deep.

The same property explains the module-argument gap recorded above: a module's
`frame:getParent().args` arrives *expanded*, which is why the
`<div class="plainlist">` it echoes back carries no `about` and costs no extra
`#mwt` ids. rustoid handed it unexpanded wikitext, so re-expanding it in the
module's output built a fresh transclusion. Expanding the value at call time
fixes both at once, and it is what `Frame::expandArg`'s
`'expandTemplates' => true` does in Parsoid's own (parser-test-only) native path;
the production path gets it from the preprocessor.

### Two things the change needed care with

**The helper frame's title.** Expanding an argument value needs a frame of its
own, and rustoid built those with `frame.new_child(frame.title().clone(), …)` —
a child whose title *equals the caller's*. An argument that calls the caller's own
template then matches that frame in `loopAndDepthCheck` and reports a loop, which
is the same class of false positive one level down. The helper is now titled with
the *page*, which can never equal a template title; Parsoid's equivalent frame
has a null title, which can never match anything.

**One more arm in the argument renderer.** With the values expanded, the text a
module receives is built from `argument_value_text`, which refused the value
outright because of an `mw:Entity` span — so the whole value fell back to a
stale source range and `Template:Redirect-several`'s link list came out as
"other terms". The renderer now emits the entity *as written* (`&#32;`) and skips
the decoded character and the end tag, mirroring PHP's `$i += 2`; re-parsing the
module's output rebuilds the same span.

### Scoreboard

```
Association football        9496 ->  6122      France              19849 -> 18416
Chernobyl disaster          5795 ->  4109      COVID-19 pandemic   10018 ->  8291
Chess                       5449 ->  3863      Megadeth             7766 ->  5613
```

No page's first difference regressed, and the fixture guard moved for the first
time this round — **876 → 877** (`tables.txt` 88 → 89), the fixed fixture being
the argument-value cell splitting that the same code path drives. The wider
corpus total *fell* 28.26 M → 27.99 M bytes: spurious loop-error markup is gone,
and values that used to re-expand in the module's frame no longer do so twice.

`Polio vaccine` stays at 5350, but what differs there changed: the category links
are no longer wrapped in the `<span class="mw-empty-elt">` the service gives
them, and rustoid's stylesheet takes `about="#mwt6"` where the service takes
`#mwt7`. Those are the id/`shouldStashRenderingTransparentNodes` thread again — a
page-local about-id is being spent one time too few — not the loop.

## A comment in a parser-function branch survived the trim

The handoff read `World War II`'s first difference (byte 939) as an
`is_deletable_in_range` bug: rustoid wrapped the leading `"\n     "` before
`{{redirect-several|…}}` in a stray `<span about="#mwt3"> </span>` where the
service wrapped nothing, and the note blamed PHP's `isDeletableNode` for
requiring a text node's content to be exactly `"\n"`. **That reading was wrong.**
The node should not have been there at all; `is_deletable_in_range` was answering
correctly for what it was handed.

The stray whitespace is a comment's fault. `Template:Redirect-several`'s outer
`#switch` has a `#default` branch that begins
`<!-- This is for if … -->\n     {{#switch:…}}`, and the same shape recurs one
level down. `#switch` (MediaWiki core's, which the production path uses) answers
`trim( $frame->expand( $valueNode ) )`, and `PPFrame_Hash::expand` appends the
**empty string** for a `comment` node in HTML output mode — so the comment, and
the blanks on both sides of it, are gone before the trim. rustoid kept the
comment as a `Comment` token, and `trim_item_edges` stops at any token that is
not a newline, so the trim never reached the whitespace: it survived as a
`"\n     "` text node inside the transclusion range, non-deletable by any rule,
and `wrap_transclusion_children` wrapped it in the single-space `about` span.

Deleting the two comments from the cached template by hand made the region
byte-identical, which is what pinned the cause. The fix is one filter: a
parser-function branch value drops its top-level `Comment` tokens in `expand_kv`,
the common path of `trimmed_branch`, `untrimmed_branch` and `branch_items`.
Nested template invocations are `SelfclosingTag` tokens at that level, so their
inner comments are left for their own expansion — matching `$frame->expand`,
which recurses only into the tree it was given.

The same probe shows the general symptom, not just the whitespace:
`A{{#if:1|a<!--c-->b}}B` served as `A…>ab</span>B` and rustoid as
`A…>a</span><!--c--><span…>b</span>B`. Both `#if` and `#switch` now render `ab`.

- `World War II` first difference: **939 → 2472**. The new difference is a
  `<p class="mw-empty-elt">` classification (the `{{Good article}}` indicator),
  a separate thread — see the next section.
- Corpus totals (offline, `/tmp/wide_r8.txt`): rustoid 27 989 325 → 27 988 859
  bytes. No page's *bucket* (first-difference kind) changed, because the pages
  that carry comments at a branch head are not the pages that differ first;
  the win is measured on the byte total and on the two reduced snippets.
- Fixture guard unchanged at **877/896**; `php/comments.txt`'s two failures are
  the pre-existing standalone skips.
- The wider corpus is currently **unreliable as a scoreboard**: 40 of 41
  compared pages report "facts 9d newer than the oracle", so the served HTML is
  a different revision than the facts being rendered. A fresh offline corpus
  needs the oracles re-pinned first.

## The missing `mw.ustring.codepoint`, and WW2's `<p class="mw-empty-elt">`

With the stray span gone, `World War II`'s first difference moved to byte 2472,
where the service serves `<p class="mw-empty-elt" id="mwBg">` and rustoid served
`<p id="mwBg">`. The natural reading — and the one already written down for
`List of sovereign states` — was that `CleanUp::isEmptyNode` was under-counting,
so the next step looked like extending `is_rendering_transparent`. **That reading
was wrong too.** Instrumenting `handle_empty_element` showed no `<p>` at cleanup
time with the oracle's child list; the element DID have one more child, a
`<strong>`, and its text was the answer:

```
Script error: lua error: … [string "mw.html"]:29: attempt to call field 'codepoint' (a nil value)
```

So rustoid had injected an error where the service renders nothing, and the
auto-inserted paragraph was non-empty *because of the error*. The `mw-empty-elt`
classification was right all along; the input was not.

`HTML_LIB` is rustoid's Lua port of Scribunto's `mw.html`, and its `cssEncode`
(line 29) escapes a non-ASCII CSS character with
`string.format('\\%X ', mw.ustring.codepoint(m))`. `mw.ustring.codepoints` (the
plural `gcodepoint` helper) existed; the singular `codepoint` did not, and it is
not in Lua's `string` either, so the field read was `nil`.

`luafn_ustring_codepoint` ports Scribunto's `UstringLibrary::ustringCodepoint`:
`i` defaults to 1 and `j` to the **original** `i`, a negative index counts back
from the end, `j < i` after that adjustment yields nothing, and both are then
clamped to `[1, len + 1]`. The return is a **multivalue** of integers, like
`string.byte` — a table would break the `string.format` call that reads it.

- `World War II` first difference: **2472 -> 4546**, and its output grew
  1 361 659 -> 1 376 696 bytes (the stylesheet is rendered instead of an error).
- The new difference at 4546 is the **encapsulation head**: the service wraps the
  infobox transclusion's leading `<style>` in
  `<span class="mw-empty-elt" about="#mwt11" typeof="mw:Transclusion">` and puts
  only `about` on the `<p>`; rustoid puts the whole `typeof`/`data-mw` on the
  `<p>`. That is the `ensureElementsInRangeAndAddAboutIds` wrapper branch already
  scoped out in "The encapsulation head, measured".
- Fixture guard unchanged at **877/896**. Any page whose templates build CSS
  through `mw.html` with a non-ASCII value now renders the sheet rather than a
  `Script error`.

### One more wrong turn worth recording

Two of this round's three diagnoses were wrong in the same way: a *symptom* of an
upstream difference was read as a bug in the pass where it surfaced. The stray
`about` span was a comment that should not have survived the branch expansion,
not an `is_deletable_in_range` rule; and the missing `mw-empty-elt` class was an
injected Lua error, not an `isEmptyNode` predicate. Both times the pass that
produced the visible byte was correct for its input. The habit that found the
cause was the same both times: render one page, look at the *whole* node the
service and rustoid disagree on, and ask what is in rustoid's copy that is not in
the service's — rather than what rule the disagreeing pass applies.

## The wide corpus is mostly measuring a half-empty cache

The third reading of `World War II` — the note that its 4546 difference is the
*encapsulation head* — was wrong as well, and this one matters more than the
other two because it says something about the corpus rather than one page.

At 4546 the service has `<span class="mw-empty-elt" about="#mwt11"
…>`, and rustoid a `<p about="#mwt11" …>`. Looking *inside* rustoid's `<p>`
answered it: its content was

```html
<extension typeof="mw:Extension" name="templatestyles"
  source='&lt;templatestyles src="Module:Infobox military conflict/styles.css">&lt;/templatestyles>'></extension>
<a rel="mw:WikiLink" href="./Template:Stack_begin" title="Template:Stack begin">Template:Stack begin</a>
```

a literal placeholder for a `<templatestyles>` whose stylesheet
(`Module:Infobox military conflict/styles.css`) was **not in the cache**, and a
redlink for a `Template:Stack begin` that was not either. Those two unexpanded
nodes are what made the auto-inserted `<p>` exist at all; the encapsulation head
was never the difference. Fetching the missing pages and re-rendering took `World
War II` from **0.77x to 1.00x** of the oracle in one step, first difference 4546 ->
**8709** (now an `about` id on a stylesheet, `#mwt12` vs `#mwt31` — the
encapsulation-head thread proper).

### The same gap dominates the whole corpus

The corpus's own report says it: **38 of 42** pages still contain a literal
`{{…}}` in rustoid's output, and 26 of 42 have more literal wikitext than the
service. A page whose inputs are missing cannot match whatever its parser does.

The fix is to populate the cache, and that is where it gets awkward: a full
online corpus run is not practical. The wiki answered **HTTP 429 Too Many
Requests** after roughly five pages, so the run skipped 42 of its 48
comparisons. The *renders* still ran for the pages it reached, so their missing
inputs were fetched — which is why five pages improved in the offline re-score
below — but the corpus cannot be populated this way in one sitting, and the
handoff's "never run a full corpus online" rule turns out to have a second reason
beyond rewriting baselines.

### Offline re-score (same 48-title corpus, same pinned baselines)

```
Albert Einstein    1521 ->  4234      2024 Summer Olympics  1628 ->  6008
Anarchism          2535 ->  6130      Nigeria               2187 -> 13411
World War II       4546 ->  8709
```

Everything else is unchanged, and the total rose 28 003 898 -> 29 112 110 bytes
(0.56x -> 0.58x): the newly expanded templates and stylesheets are content that
was previously a placeholder. **The byte total growing is the improvement here**,
which is the clearest statement of how misleading the 0.56x ratio was as a
progress bar. The pinned `html:` baselines were not touched — the run's own
message is `keeping pinned html:World War II (rev Some(1375790811)); refused to
overwrite with rev Some(1377143094)`.

So the next session's first move should be to populate the cache (slowly, well
inside the rate limit, and resumable) before trusting any page-level scoreboard.
`Module:Math` (byte 1, a `Module:` content model), `Python` and
`ISO 3166-1 alpha-2` (byte 578/598, a protection-tracking category that the live
API now says is *correct* — the page lost its protection since the oracle was
pinned) are the smallest offsets but all three are unfixable as measured.

## Populating the cache exposed an alphabetized `data-mw`

Two pages got *worse* when their inputs were fetched, and that turned out to be
the most useful thing the populate did. `Doom (1993 video game)` moved 5543 ->
2163 and `Grand Theft Auto V` 3895 -> 2189, both landing on the same shape:

```
parsoid: {"parts":[{"template":{"target":{…},"params":{"title":…,"image":…,"alt":…},"i":0}}]}
rustoid: {"parts":[{"template":{"i":0,"params":{"alt":…,"artist":…,"caption":…}}}]}
```

Every key is in **alphabetical** order, and `target` has moved after `params`.
That is not Parsoid's order, which is a fixed schema (`target`, `params`, `i`) with
the parameters in *source* order.

The cause is a serialization round-trip. `prepend_unwrapped_wt_part` and
`append_trailing_wt_part` (the `recordTemplateInfo` leading/trailing-wikitext
steps) parse a `data-mw` string into a `serde_json::Value`, edit it, and print it
back with `to_string()`. `serde_json`'s default `Map` is a `BTreeMap`, so that
round-trip re-sorts every object in the document at once. The templates that
reach it are the ones whose range carries an `unwrappedWT`; the fetch is what got
the corpus far enough into those ranges to see it.

The fix is `features = ["preserve_order"]` on the workspace's `serde_json`, which
makes `Map` an insertion-ordered map. The code had already assumed this: the
comments on `serialize_template_info` ("the order has to survive serialization") and
on `json_from_lua` ("keys keep the order Lua iterated them in … `json_encode` then
preserves that order") both describe insertion order, and the second is a place
where the `BTreeMap` silently contradicted the stated intent.

- `Doom (1993 video game)` 2163 -> **5529** and `Grand Theft Auto V` 2189 ->
  **3881** — i.e. back past their pre-populate 5543/3895, so the populate is a net
  gain now that the ordering bug it exposed is fixed.
- Fixture guard unchanged at **877/896**; the full workspace suite passes, so
  nothing that asserts on JSON order depended on the sort.

### Session-end scoreboard (offline, `wide`, 41 compared)

With the comment drop, the `codepoint` fix, the partially populated cache and the
JSON order fix all in, and against the *same pinned baselines* throughout:

```
session start  rustoid 27 988 859 bytes (0.56x), WW2 939
session end    rustoid 30 024 468 bytes (0.60x), WW2 8709

World War II            939 -> 8709      Nigeria            2187 -> 13411
Commonwealth of Nations 1508 -> 8310      2024 Summer Olympics 1628 -> 6008
Polio vaccine           5350 -> 5350      (unchanged - blocked on drift)
Anarchism               2535 -> 6130      Albert Einstein    1521 -> 4234
Doom                    5543 -> 5529      Grand Theft Auto V 3895 -> 3881
Bitcoin                 2978 -> 3038      Brat (album)       1610 -> 1621
```

No page regressed, and the byte total *rising* is the point: the extra 2.0 M
bytes are templates and stylesheets that were previously literal `{{…}}` and
`<extension>` placeholders. The first differences that moved did so because the
cache (or the parser) stopped being the limit, not because a rule changed.

## The `{{…}}` counter was lying, and `-{{{` is not a variant opener

Continuing the populate hit the rate limit again (a fifth pass moved nothing at
all), so the next step was to look at *what* the 38/42 literal-`{{…}}` pages
actually contained. The first thing that came out is that the metric itself is
contaminated.

`Unexpanded::count` decides "inside a tag" by toggling on every `<` and `>`. A
`data-mw` attribute may contain a raw `>` — HTML allows it, and Parsoid emits it
(it escapes `<` as `&lt;` but leaves `>` alone, and so does rustoid) — so the
first raw `>` inside a large attribute ends the alleged tag and the rest of the
attribute is counted as document text. `World War II`'s navbox `data-mw` is
241 KB with 518 raw `>`, and the harness counted the whole thing. Re-counting
with an HTML-aware scan gives rustoid **18** literal `{{` in real text against
the oracle's **2** — not 882, and not "raw wikitext": the page prose rendered
fine.

### The 17 were one template: `Template:Flag icon/core`

They were all the same construct, leaked as `<templatearg>` elements:

```
[[File:{{{flag alias-{{{variant}}}|{{safesubst:#if:{{{flag alias|}}}|{{{flag alias}}}|Flag placeholder.svg}}}}}|…]]
```

The parameter **name** is itself a nested argument (`flag alias-<variant>`) and
the part after the top-level `|` is its default. rustoid's splitter never split
it, so the token came out as one `Str` key with no default — printable, but
unmatchable, so `{{flagicon|Soviet Union}}` rendered literal `[[File:…]]` where
the service renders a flag `<img>`.

The cause is a one-character confusion in `split_template_args_impl_offsets`: the
`-{` of `alias-{{{variant}}}` was read as a `-{ … }-` language-variant opener.
Nothing ever closes it (there is no `}-`), so `dash_brace` stayed at 1 and every
later top-level `|` counted as protected.

The fix keeps the variant case working and only stops `-{` from firing when it is
the tail of a brace run — `-{` immediately followed by `{` is the hyphen that
ends a parameter name followed by a nested `{{{`, not a variant opener.

### Verified against the service, not against the fixture

| input | service | rustoid before | rustoid now |
| --- | --- | --- | --- |
| `{{#if:1|-{x\|y}-\|z}}` | `-{x\|y}-` | `-{x\|y}-` | `-{x\|y}-` |
| `{{#if:1\|p-{{{l\|q}}}\|D}}` | `p-q` | `p-q\|D` | `p-q` |
| `{{flag icon/core\|alias=Soviet Union\|flag alias=…}}` | flag `<img>` | literal `[[File:<templatearg…>` | flag `<img>` |

- `World War II` real-text literals **18 → 4** (the oracle has 2; one of the four
  is the `{{cite journal}}` the oracle has as well).
- Corpus total 30 527 733 → **30 564 312** bytes (+36.6 KB of flags that were
  placeholders). **No first difference moved** — the flags sit after each page's
  first difference — and the fixture guard stays **877/896**.
- What is left on `World War II` is the **navbox**: three literals, and they come
  with raw CSS text (`…crossreference{padding-left:0}{{{content}}}Belligerents…`)
  and `{{{name}}}`, i.e. a navbox emitting a stylesheet's *content* without its
  element and leaving its own `content`/`name` parameters unsubstituted. That is
  a `Module:Navbox` output bug and the next thing to reduce.
- `World War II`'s first difference is now `about="#mwt12"` vs `#mwt25` on the
  infobox's leading `<style>` — the extension/encapsulation numbering thread.
  (The handoff's end-of-session note had `#mwt31`; on the current tree and cache
  the same first difference reads `#mwt25`.)

## A live HTML element in a module argument, and the stale source range it exposed

The last `World War II` literals were `{{{content}}}` inside a crossreference
hatnote. The reduction is exact and small:

| input | service | rustoid before | rustoid now |
| --- | --- | --- | --- |
| `{{hatnote inline\|1=PLAIN}}` | `PLAIN` | `PLAIN` | `PLAIN` |
| `{{hatnote inline\|1={{nowrap\|SEE}}}}` | `<span class="nowrap">SEE</span>` | `{{{content}}}` | `<span class="nowrap">SEE</span>` |
| `{{hatnote inline\|1={{lc:SEE}}}}` | `see` | `see` | `see` |

### The chain

`Template:Hatnote inline` is `{{#invoke:Hatnote inline|hatnoteInline|1={{{1|{{{text|{{{content}}}}}}}}}|…}}`.
`Module:Hatnote inline` passes `frame:newChild{ args = args }` to `Module:Hatnote`,
whose own `getArgs` uses `{ parentOnly = true }` — so what matters is the frame's
arguments, handed to the module as *strings*.

The `#invoke` argument `1` reaches `expand_invoke_args` **already substituted**:
`Frame::expand` on the template body replaced the `{{{1|…}}}` reference with the
caller's value (the `{{nowrap|SEE}}` tokens, in turn expanded to
`<span class="nowrap">`, `SEE`, `</span>`). So `expandable_content` reports no
template and no argument reference, and `expand_invoke_args` leaves the value
alone. Then `expanded_arg_pair` → `argument_value_or_source` →
`argument_value_text([span, "SEE", endspan])` returns **`None`**, because a live
HTML element was deliberately declined — and `kv_value_source` falls back to the
value's `src_offsets`, which still spell the **unsubstituted**
`{{{1|{{{text|{{{content}}}}}}}}}`. `Frame::expand`'s `expand_in_attributes`
replaced the value's *tokens* but kept the recorded range, so the two no longer
agree. The module was handed the raw reference.

### What was misleading

The first instrumentation printed `TAKEYS miss name="1" … keys=[]` from a frame
titled `Template:Hatnote inline`, which read as "the body frame has no
arguments" and sent the search after `new_child`. The frames the module actually
reads all had `keys=["1"]`; the empty-args frame was the **module-output** frame
re-expanding the literal `{{{content}}}` the module had already echoed back. Once
the *call* argument's resolved text (`READ src=Call idx=0` →
`"{{{1|{{{text|{{{content}}}}}}}}}"`) and its rendered input (`INVKV key="1"
text="SEE "`) were printed side by side, the stale range was the only
possibility left.

### The fix

`argument_value_text` renders a live `Tag`/`EndTag` from its `data_parsoid.src`,
which is what every other token→source path in rustoid does
(`token_to_source`, `tag_content_source`, `tokens_to_string`). A token with no
`src` still declines, so the `<div>X</div>` fallback the docstring describes is
unchanged, and nothing that used to render can render worse: for an *unmodified*
value the source range and the per-token `src` are the same text. Unit test
`an_argument_value_renders_a_live_element_from_its_source`.

### Measured

- `World War II` `{{{` literals **3 → 0**.
- Fixture guard unchanged at **877/896**; workspace tests, `clippy
  --workspace --all-targets` and `cargo fmt --all --check` all clean.
- The page's *first* difference does not move — it is the `#mwt12`/`#mwt25`
  numbering above, which sits earlier in the document than the navbox.

## The extension-numbering order: the probes, and the template-argument fix

With the literals gone, `World War II`'s first difference is the `about` id on
the infobox's leading `<style>`: the oracle writes `about="#mwt12"`, rustoid
`about="#mwt25"`. The ids before it are identical on both sides
(`1,2,3,4,5,7,9,10,11` — the gaps are ids spent on things that do not render), so
rustoid has spent **13 extra ids** between the 11th and this stylesheet.

### Measured how

`RUSTOID_TRACE_ABOUT=1` makes `new_about_id` log every allocation, and
`examples/render` (with `RUSTOID_WRAP_SECTIONS=1`, the harness's options) then
reproduces the corpus render offline. Extracting `about="#mwtN"` from each side
*in document order* is the comparison that matters, because the two orderings
can differ while the totals look plausible:

```
oracle  styles by id: 12 Infobox-mc, 13 Stack, 14 Multiple-image, 15 Hlist,
                     18 Plainlist, 19 Crossreference, 26 Navbox, 28 Navbar, 32 Sidebar
rustoid styles by id: 25 Infobox-mc, 27 Multiple-image, 28 Hlist, 30 Plainlist,
                      32 Crossreference, 37 Navbox, 39 Navbar, 46 Sidebar, 90 Stack
```

Two things fall out. Every rustoid id is shifted by the 13, and `Stack/styles.css`
— the second stylesheet in the document — is drained 90th by rustoid.

### What the oracle actually does

The obvious theory, "the oracle dedupes before numbering and rustoid numbers the
duplicates", is **wrong**: the oracle's dedup placeholders keep their id. It emits
399 `mw-deduplicated-inline-style` links on this page and each carries an `about`
(`#mwt20`, `#mwt21`, …), so it spends ids on the duplicates too. The divergence is
**order**, not count.

`src/Wt2Html/TT/ExtensionHandler.php::onDocumentFragment` allocates
`$env->newAboutId()` for every extension but `nowiki` as that extension's DOM
fragment is produced, and `src/Wt2Html/DOM/Handlers/DedupeStyles.php` replaces a
duplicate `<style>` with a `<link>` that copies the original's `about` (line 44)
— so the duplicates spend ids too, exactly as the first occurrences do.

### The rule, measured against the live endpoint

The oracle is **not** pure document order, and the paragraph that used to stand
here (claiming it was) was wrong. Probed through `--wikitext` against the
transform endpoint:

| input | ids (service) |
| --- | --- |
| `<templatestyles …/>{{Center\|b}}` | templatestyles `#mwt2`, Center `#mwt1` |
| `{{Center\|b}}<templatestyles …/>` | Center `#mwt1`, templatestyles `#mwt2` |
| `<templatestyles …/>{{Center\|b}}{{Center\|c}}` | Center `#mwt1`, Center `#mwt2`, templatestyles `#mwt3` |
| `{{#if:1\|<templatestyles …/>{{Center\|b}}}}` | Center `#mwt1`, templatestyles `#mwt2` |
| `{{Hatnote\|SEE}}<templatestyles …/>` | Hatnote `#mwt1`, `Module:Hatnote/styles.css` `#mwt2`, Plainlist `#mwt3` |

So the rule is **per chunk: the chunk's transclusions take their ids first (in
document order), then that chunk's extensions** — and it holds for nested chunks
(the `#if` branch obeys it too). That is what `number_style_placeholders`
implements at the end of `expand_templates`, and it is why the cheap fix below is
not one.

### The wrong turn: deferring styles to the tree builder

`DEFERRED_ABOUT` in `tree_builder_html.rs` looked like the fix — stash each
`<style>` with the marker and let `resolve_deferred_about_ids` take the id in
document order at the splice point. It was implemented and measured, and it is
**wrong**: it places every stylesheet after *all* transclusions, so `World War
II`'s first stylesheet moved from `#mwt4` to `#mwt1360` (the page spends ~1359
transclusion ids). The change was reverted; `DEFERRED_ABOUT` stays unused.

### The fix: a template argument's extensions are numbered where the value lands

The trace does not stop at "interleaved"; it names the interleave. rustoid
numbered a value's stylesheets while expanding the value — in the *caller's*
frame, before the callee runs — so they took ids ahead of the extensions the
callee emits, which are earlier in the output. The guard that already existed for
`#invoke` arguments (`expand_invoke_args`, `Parser::arg_expansion`) was simply
missing for a *template's* argument values (`expand_template_arg_values`).

It is not the same guard, though. An `#invoke` argument is rendered back to
text and its extensions re-created where the module's output places them, so
both the id and the *indicator* spend move. A template argument is spliced into
the callee's body, so its extensions are numbered where the value lands — using
the one `.saturating_sub(1)`-guarded counter for both was wrong: it also dropped
the indicator spend (`#mwt6` on `World War II`) that the service makes. So
`Parser::style_defer` is a separate counter, and `ArgExpansion` decrements both.

- One counter for "extensions numbered where the value lands" (`style_defer`),
  set by `expand_template_arg_values` (`begin_style_defer`) and implied by
  `begin_arg_expansion`; `expand_templates` skips its extension post-pass while
  it is non-zero.
- The indicator spend stays tied to `arg_expansion` alone, so a template
  argument's `<indicator>` is still numbered with its transclusion.

### Measured

- `World War II`'s first difference moves **8709 → 9061**: the infobox's
  `Module:Infobox military conflict/styles.css` is now `#mwt12` (the service's
  value), where it was `#mwt25`.
- Corpus (offline, 48 titles): rustoid **30 495 534 → 30 502 198** bytes
  (+6 664 of previously mis-numbered/mis-placed markup), and eight pages advance
  their first difference past the old one (`COVID-19 pandemic`, `Megadeth`,
  `The Beatles` → `expanded-attrs`; `Chess`, `France`, `Nigeria`, `Nobel Prize`,
  `Quicksilver (film)` → `media`).
- Fixture guard **877/896**, workspace tests, `clippy --workspace --all-targets`
  and `cargo fmt --all --check` all clean.

### The Stack fragment, at first

The ids match the service through the whole infobox — `STYLE-NUM` shows `frag15`
infobox `#mwt12`, `frag16` Stack `#mwt13`, `frag17` Multiple image `#mwt14`. But
`Stack/styles.css` was **created twice**: the `#mwt13` fragment was spent and then
never emitted, and a second fragment was built at `#mwt64`.

### That double creation, and its fix

The first reduction is exact: `{{stack begin|clear=true}}A{{stack end}}` emitted
one Stack stylesheet on the service and two on rustoid. `Stack/styles.css` is a
**literal** `<templatestyles>` at the top of `Template:Stack`, and the template is
reached not from the page but from a module — `Module:Infobox` builds the infobox
with `frame:expandTemplate('stack begin')`. So the fragment was built while the
request was answered, and then **dropped from the answer**: the `ExpandTemplate`
branch of `expand_lua_request` rendered it with
`lua_deferred::render_answer`, which has no textual form for an `mw:DOMFragment`
placeholder, while the `CallParserFunction` branch two arms above already used
`render_answer_markers`. The answer lost the stylesheet, the module put the
*unresolved* source back in its output, and the output re-expanded it — a second
fragment, a second id.

One line: the `ExpandTemplate` branch now returns
`self.render_answer_markers(&encapped)`, matching the parser-function branch.

The first attempt at this was the wrong lever and is recorded above: routing a
*value* that holds a fragment through `render_answer_markers` in the arg-text
renderer, which renders the whole value through the weaker `render_answer` and
lost ~18 KB. The placeholder has to be preserved where it is *produced* (the
answer), not where the value is later flattened.

Measured: `stack begin` emits one stylesheet again; World War II's first
difference moves **9061 → 9924**; corpus 30 502 198 → **30 565 629** bytes
(+63 KB of fragments that were being dropped), three pages advancing. Fixture
guard 877/896.

### What is left

The new first difference at 9924 is not the gate. It is the Stack class:

```
service: <div class="stack mw-stack stack-clear-right" about="#mwt11" …
rustoid: <div class="stack mw-stack stack-right"        about="#mwt11" …
```

`Template:Stack` writes `class="stack mw-stack {{#switch:{{{clear|}}}|left|true=clear-}}right"`,
and rustoid reads `clear` as unset where the service sees `true`.

**Resolved** — see "The Stack class" below; it was two defects on the module
answer path, not the class itself.

### The `mw:ExpandedAttrs` gate, finished

The gate is a per-token flag, as the earlier section planned, and not more
`in_template` plumbing. `TempData::synthesized` is set on the top-level items a
template body or a module output produces (`mark_synthesized`, called at the end
of `expand_one_template` and on the `#invoke` output), and `build_expanded_attrs`
skips the marking when it is set — Parsoid's `AttributeExpander` runs *per chunk*
and a body's `inTemplate` suppresses the marking, while rustoid's
`expand_attributes` runs once over the flattened page stream.

That the coarse marking (every top-level item of the body, including a tag that
arrived as a template *argument*) is faithful was checked, not assumed: the
service does not mark a page-authored tag that travels through a passthrough
either. `{{Plain list|1=<div class="{{#if:1|a}}">y</div>}}` renders
`<div class="a">` on both sides, unmarked.

Measured:

- The minimal probe matches the service: no `typeof="mw:ExpandedAttrs"`, no inner
  wrapper `<div>`.
- `World War II` emits **18 → 7** `mw:ExpandedAttrs` (the oracle has 4). The 11
  removed were all body attributes — `.stack`, `.hlist`, `.plainlist` divs whose
  `class` was written literally with a nested template in a template body. The 7
  that remain are all `typeof="mw:Error mw:File mw:ExpandedAttrs"`, a *different*
  and pre-existing gap (a missing-file lookup), and the oracle's own 4 came from
  attributes holding a `<span typeof="mw:Nowiki">` or a chosen external-link
  `href` — cases rustoid did not produce before the gate either.
- Corpus (offline, 48 titles): **30 565 629 → 30 401 269** bytes, i.e. −164 KB of
  body markup the service does not emit either.
- Fixture guard **877/896**; workspace tests, `clippy --workspace --all-targets`
  and `cargo fmt --all --check` clean.

### The wrong diagnosis this section used to carry

The paragraph that stood here said the first difference at 9924 *was* the gate,
and — worse — that the gate had introduced the class mismatch. Both were wrong,
and a `git stash` of the gate settled it: **with and without the change**,
`World War II`'s first difference is at byte 9924 and it is the
`stack-clear-right` / `stack-right` class. The class bug was simply masked in the
harness's scoreboard, whose classifier scans the ±400-byte context for keywords:
before the gate that context still contained `mw:ExpandedAttrs` (the wrapper on
the very next bytes), so the report said `expanded-attrs`; after the gate the
context no longer does, and the same byte is now reported as `table`. The kinds in
`/tmp/wide_markers.txt` vs `/tmp/wide_ea.txt` are that artefact, not a regression.

## The Stack class: a module answer is rendered from stale source

With the gate in, `World War II`'s first difference is the Stack class —
`stack-right` where the service has `stack-clear-right` — and a `git stash` of
the gate proved it pre-existing, not introduced by it (the classifier had merely
been reading the adjacent `mw:ExpandedAttrs`).

`Template:Stack` writes `class="stack mw-stack {{#switch:{{{clear|}}}|…}}"`, and
the infobox reaches it through `Module:Infobox military conflict`:
`frame:expandTemplate{ title = 'stack begin', args = { clear = 'true' } }`. Two
independent defects sat on that path, and the reduction that separated them was a
pair of throwaway templates read through a throwaway module (cached under a
scratch `RUSTOID_CACHE_DIR`, so the real cache is untouched):

| module call | rustoid before | expected |
| --- | --- | --- |
| `{{Echotop}}` (top-level `{{{foo}}}` and `{{#switch:{{{foo}}}}}`) | `BAR/YES` | `BAR/YES` |
| `{{Echoattr}}` (`<div class="x{{{foo\|MISSING}}}">`) | `xMISSING` | `xBAR` |
| `{{Echoswitch}}` (`<div class="{{#switch:{{{foo\|MISSING}}}\|BAR=YES\|NOPE}}">`) | `NOPE` | `YES` |

So a *top-level* reference in a body resolved, but a reference **inside an
attribute** did not — and one that sat inside a parser function in that
attribute did not either.

**Defect one — the attribute's templates were never expanded.** `Frame::expand`
substitutes `{{{…}}}` everywhere in the body, attributes included, so the
`{{#switch:…}}` became `{{#switch:BAR|…}}` — but the switch itself is a
*template*, and on a page the templates inside attributes are expanded by the
one `expand_attributes` pass at the end of `build_ast`. The module is handed the
body's wikitext *before* that pass, so the answer still spelled
`{{#switch:BAR|BAR=YES|NOPE}}`, and re-parsing it — with no frame — answered
`NOPE`. The fix is the pass the analogous argument paths already run
(`expand_invoke_args`, `expand_template_arg_values`): `expand_lua_request` now
calls `expand_attrib_templates` on the expansion before rendering it.

**Defect two — the answer was rendered from the source, not the expansion.**
`render_answer` renders each token through `tokens_to_source`, which uses
`data_parsoid.src` — the source *as written*. After the first fix the switch was
expanded to `YES`, but the tag's `src` still read
`<div class="{{#switch:{{{foo|MISSING}}}|…}}">`, so the module got the
unexpanded form back. Each attribute carries the source it was written as
(`vsrc`/`ksrc`), so
[`rewrite_expanded_attrs`] replaces that text with the attribute's *current*
value and leaves the rest of the tag alone. Where the old text is not unique in
the tag the rewrite is declined rather than guessed, so nothing is corrupted.

The probes now answer `xBAR` / `YES` / `stack-clear-right`, and on `World War II`
the first difference moves **9924 → 10413**.

## The templatestyles `wrapper`

The next difference at 10413 is the `Multiple image/styles.css` stylesheet. The
service scopes it to `.mw-parser-output .tmulti`, adds `"wrapper":".tmulti"` to
its `data-mw` attrs, and keys it
`TemplateStyles:r1349637415/mw-parser-output/.tmulti`; rustoid did none of the
three. `templatestyles::render` already took a `wrapper`, but used it to
**replace** `.mw-parser-output` — a guess, and wrong.

The rule was read off the transform endpoint rather than the CSS spec
(`<templatestyles src="Plainlist/styles.css" wrapper=".foo"/>`):

- the scope is `.mw-parser-output` **plus** the wrapper (`.mw-parser-output .foo`),
- the dedup key gains `/mw-parser-output/{wrapper}`,
- the `data-mw` attrs carry `wrapper` after `src`.

`Module:Multiple image` passes `wrapper = ".tmulti"` and wraps its output in
`<div class="tmulti">`, which is what the extra scope is for. The change touches
`render`, `style_node` (now takes the wrapper) and `PendingStyle`, which carries
it from the `<templatestyles>` tag to the stashed `<style>`.

Measured together: `World War II` moves **10413 → 12542** (the new difference is
the multiple-image width, `100px` against the service's `292px` — a separate,
image-metric bug). Corpus (offline, 48 titles) 30 401 269 → **30 408 898** bytes;
only `World War II` and `Zebro` change first-difference kind, both moving forward
past the stylesheet. Fixture guard **877/896**; workspace tests, clippy and fmt
clean.


## The multiple-image width, and `mw.html.create('')`

The 12542 difference is `Module:Multiple image`'s row width: rustoid wrote
`width:100px` where the service has `292px`, and `width:nanpx` on every cell.

`getdimensions` (the `total_width` path) reads the file's natural size from
`mw.title.new('File:…').file`, whose `width`/`height` rustoid did not implement —
so every width was `0`, the aspect-ratio division was `0/0`, and `nan`/`inf`
propagated into the style attributes.

`TitleFacts` now carries `file: Option<FileDims>`, filled by `title_facts_of`
for a `File:` title (the same place, and the same retry signal, as `exists`: a
file the host was never asked about is requested and answered next round).
`title_derived_field` answers `"file"` only in the File namespace, so a
non-file title has no `file` and asks for none.

Then the cells still carried `&lt;>` … `&lt;/&gt;`: `renderImageCell` builds its
cells with `root = mw.html.create('')`, a tagless *container*, and `Node:_render`
tested `self._tag ~= nil` — the empty string is not nil, so it emitted a literal
`<>` around every image cell. Scribunto documents the blank tag name as an empty
node; `_render` now treats `''` as no tag.

Measured: the widths match (`292px`, `171px`, `115px`), the `<>` wrappers are
gone, and World War II's first difference moves **12542 → 12652 → 12711**.
Corpus (offline, 48 titles) 30 408 898 → **30 377 979** bytes — the `</>` pairs
were pure invention, so the total moves toward the service. Fixture guard
**877/896**; workspace tests, clippy and fmt clean.

The difference at 12711 is `typeof="mw:Error mw:File"` where the service has
`typeof="mw:File"`: the *sized* thumbnail entry (`…@w171`) is not in the offline
cache, because the width bug meant it was never requested before. That one is a
cache-completeness artefact rather than a parser defect; an online run would
populate it.

## Backfilling file metadata without re-pinning anything

The `mw:Error mw:File` at 12711 was a cache gap: the width fix made rustoid
request a thumbnail size (`…@w171`) that no earlier run had ever asked for, so
the offline cache had no entry and the media processor fell back to broken-media
markup. The obvious fill — an online page run — is the one thing not to do here:
wikitext bodies are re-fetchable in the cache model (only `Rendered` baselines
are guarded), so an online page run would silently re-pin `page:World War II`
and every template it touches to *current* revisions, orphaning the pinned
oracle.

File metadata does not have that hazard: a thumbnail is a revision-stable fact,
so a miss can be filled without touching any page. `CachedDataSource` therefore
grew one gated field, `fill_files` (set by `RUSTOID_FILL_FILES`), which lets
`get_file_info` fetch and cache a miss *even when the run is offline* — and only
that method. Every page/template/module/page-info fetch keeps its `offline`
guard, so the run still cannot touch a body.

Filling is one paced (120 ms) request per file, and additive: the WW2 render
added 13 `file:` entries and changed no other entry kind. Corpus-wide it moved
exactly the two pages that had a file-info miss ahead of their difference:

```
World War II   12617 -> 14983  (+2366)
Zebro           1495 ->  5358  (+3863)
```

and nothing else. A second run, now purely offline, reproduced every page's
first difference byte-for-byte, so the cache is complete for this corpus; its
total differed by 182 bytes, all *after* the first differences (a file fetched
for one page mid-run changes the id allotment in another, which is downstream of
content that already diverges).

World War II's difference at 14983 is the next defect and is *not* a cache gap:
rustoid emits `<span typeof="mw:File" data-mw='{"attribs":[["alt",{"txt":"in
the"}]]}' id="mwEw">` where the service has a bare `<span typeof="mw:File">`.
The media span's `alt`, built from a template argument the module passed through,
is being marked `mw:ExpandedAttrs` — the same over-marking the gate fixes for
template bodies, but on a span the media handler builds, which no `synthesized`
flag reaches.

## The media span's `data-mw`: an option is consumed, not copied

At 14983 rustoid emitted
`<span typeof="mw:File" data-mw='{"attribs":[["alt",{"txt":"in the"}]]}' id="mwEw">`
where the service has a bare `<span typeof="mw:File">`. A live probe made the
rule plain (`[[File:Example.jpg|alt=in the]]` on `Sandbox`):

```
parsoid: <span typeof="mw:File" id="mwAw" data-parsoid='{"optList":[{"ck":"alt","ak":"alt=in the"}],…}'>
rustoid: <span typeof="mw:File" data-mw='{"attribs":[["alt",{"txt":"in the"}]]}' id="mwAw">
```

The service records a plain option in `data-parsoid.optList` — which the harness
strips — and reserves `data-mw.attribs` for *expanded* options. `AddMediaInfo`
then **consumes** the options it folds into the element:
`WTSUtils::getAttrFromDataMw( $dataMw, 'alt', $keepAltInDataMw )` with
`$keepAltInDataMw = !$isImage || $errs` — so for an error-free image the `alt`
is read *and removed*, and `unset( $dataMw->attribs )` drops the field when the
list is left empty.

rustoid read `data-mw.attribs` but never wrote the consumption back, so the
container kept a blob — and an `id` with it — that the service does not emit.
`remove_data_mw_attrib` now mirrors the removal, at the end of the error-free
image path.

Measured: World War II 14983 → **19807**; corpus 30 558 484 → 30 488 901 bytes
(the removed `data-mw`/`id` pairs are pure invention). Six pages advance.

## Two CSS normaliser rules, and a bug in the guard they hid behind

The next difference at 19807 is inside `Hlist/styles.css`, in three shapes:

| source | rustoid | service |
| --- | --- | --- |
| `content:"\a0· "` | `content:"\a0· "` | `content:"\a0 · "` |
| `content:" " counter(listitem) "\a0"` | `…" " counter(listitem) "\a0"` | `…" "counter(listitem)"\a0 "` |
| `content:" (" counter(listitem) "\a0"` | `…" (" counter(listitem) "\a0"` | `…" ("counter(listitem)"\a0 "` |

- **A hex escape is re-serialised with its terminating space.** The sanitiser's
  tokeniser consumes the whitespace that *ends* `\a0`; its serialiser writes one
  back, so `\a0· ` becomes `\a0 · ` and `\a0"` becomes `\a0 "`.
- **A string and a function are self-delimiting**, so the serialiser glues them:
  `" " counter(…)` → `" "counter(…)`, `counter(…) "\a0 "` → `counter(…)` + `"\a0 "`.
  The second half is why `normalise_value` now tracks string state — its arms
  drop separators, and inside a string every character is content.

Chasing the second half exposed a **latent bug** in the first: the `)` arm found
the next non-space with `lookahead.by_ref().take_while(|n| *n == ' ').count()`,
and `take_while` *consumes* the first item that fails the predicate — so the
`next` it read was one character too far. It happened to work for the alphabetic
case it was written for (the char after the spaces was alphabetic too), and the
lookahead now uses `peek` so it cannot.

Measured: World War II 19807 → 21103 → **21353**; the `Hlist` sheet now matches
on all three. Fixture guard **877/896**; workspace tests, clippy and fmt clean.

### The first-difference report now carries a percentage

`first difference at byte 21353 of 1783587 (1.20% of parsoid) / …`. The offset
alone says nothing about how far into a page the two sides still agree, and the
percentage is what makes the scoreboard scannable.

After this run: `World War II` 1.20%, `Zebro` 6.79%, `Anarchism` 0.77%,
`List of sovereign states` 0.50%, `Tropical cyclone` 0.36%, `The Beatles` 0.47%;
the smallest remaining divergence is still `Help:Introduction` at 22.73% of a
22 KB page.

### What is left at 21353

The `<div class="hlist ">` content: the service emits `\n<ul><li>…`, rustoid the
literal `* German …`. A `* ` list inside that div was never recognised as a
list, which is a list-parsing issue in a value that arrives from a template, not
another stylesheet detail.

## Template-argument values are tokenized as blocks, not inline comments

That hlist was the visible symptom of a tokenizer that tokened a template
argument *value* as inline content only. The value is `\n* a\n* b\n`, and both
the `{{flatlist|…}}` call on the page and `Template:Flat list`'s
`{{#if:{{{1|}}}|\n{{{1}}}\n</div>}}` push it through the same reader, so the
reduction is just the loose wikitext (against the transform endpoint):

```
{{#if:1|\n* a\n* b\n}}
```

The service answered a two-item list; rustoid answered
`<ul><li>a</li></ul><span> </span><p>* b</p>` — the **first** line became a list
item and the second did not. That shape is `TokenStreamPatcher`'s T2529 hack
firing on one string: a marker meta precedes `Str("* a")`, so the T2529 branch
re-tokenizes `"* a"` alone; the next `Nl`/`Str("* b")` arrive after it and are
left as text (see `token_stream_patcher.rs`).

The root cause was one rule read short. PHP's grammar defines

```
template_param_value = template_param_text<equal=false, …>
template_param_text  = (nested_block / newlineToken)+
```

so a value is a sequence of **blocks**; the `sol` rule consumes a leading newline
and emits it as an `NlTk`, and the block that follows may be a list, a heading or
an hr (tables are suppressed by the `table=false` flag). rustoid's
`tokenize_template_arg_value` instead looped inline-only — it emitted the
newlines (which is why the value was already a `Tokens`, and why the
all-`Str` guard in `re_tokenize_sol_prefix` never fired for `{{1x|\n* a\n* b\n}}`
either) but never tried a block line. `nested_block` was the missing half.

The loader now tries `sol` + `block_line` (heading / list_item / hr, no table)
before falling back to inline content, mirroring `(nested_block / newlineToken)`;
a failed attempt is rolled back so the newline is still emitted by the
`newlineToken` branch. `find_arg_separator_eq` already modelled the heading
case (a `=` at start of line opens a heading rather than a `name=value`
separator), so the two agree. Verified against the endpoint: for
`{{#if:1|\n* a\n* b\n}}`, `{{1x|\n* a\n* b\n}}` and `{{flatlist|\n* a\n* b\n}}`
all three shapes now render the two-item list the service does; a heading inside
an argument (`{{1x|new\n=== test ===\nline}}`) also now produces the `<h3>` and
section split the service does, which the old inline loop left as literal text.

`test_template_arg_heading_not_split` was asserting the old behaviour (the
heading left as text); it is updated to assert the heading **token** and keep its
real point — that the line is not split at `=` (the argument stays positional).

### Measured, and what is still wrong

The first-difference offsets do not move, because every page's earliest
divergence is *earlier* than its hlist; the fix is a downstream correction. The
fixture guard holds at **877/896**, workspace tests, clippy and fmt are clean,
and rustoid's total output size shifts on the pages that use these templates.

Two smaller divergences sit right behind the fixed one, both visible on the same
reduction and both left for a follow-up because they share one cause — the list
items born inside an argument value carry source ranges *relative to the value
string*, so `compute_dsr` drops them and `CleanUp::trimWhiteSpace` has no `dsr` to
record its trim on:

- the service keeps the value's leading newline before the list
  (`<div class="hlist ">\n<ul>`); rustoid trims every leading `Nl` of the branch
  in `trimmed_branch`, including the one that came from the substituted value;
- the first item's leading space is trimmed in rustoid but the second item's is
  not (`<li> b</li>` where the service has `<li>b</li>`), the same missing-`dsr`
  gap seen from the other side.

Neither is cosmetic for a page that uses `{{flatlist}}`/`{{hlist}}`, so the next
step is to give the argument-value tokens a source range that survives the
DSR pass (or to run the trim against the value's own source) rather than to
paper over either symptom.

## Cite's anonymous-ref ids: the `-0` was never there

`2024 Summer Olympics` was the one corpus page whose earliest divergence was a
Cite id. The service emitted `id="cite_ref-1"` and
`body:{"id":"mw-reference-text-cite_note-1"}`; rustoid emitted
`cite_ref-1-0` and `cite_note--1`. The port had read the scheme off *named* refs
(`cite_ref-Badenhorst2019_1-0`, which is correct) and generalised the suffix to
anonymous ones, where it is not.

Cite's `AnchorFormatter` states the rule exactly:

```php
// getNoteIdentifier: anonymous
$id = $globalId;                       // cite_note-1
// getBacklinkIdentifier: 
$id = $name ? normalize("{$name}_$globalId") : $globalId;
if ( $name || $count > 1 ) { $id .= '-' . ( $count - 1 ); }
```

so an anonymous *first* use is `cite_ref-1` — there is no `-0`, because Cite
cannot know yet whether the ref will be reused. A later use does get `-1`, `-2`;
a named ref always carries the suffix (its marker is keyed by name and number from
the start). `normalizeFragmentIdentifier` also collapses a run of `_`/whitespace
to one `_`, which the old `name.replace(' ', "_")` did not, so
`cite_ref-a__1-0` was possible.

Fixed in `ext::cite::marker_id`/`note_id`, with `id_segment` replaced by a
`normalize_fragment_identifier` that matches the PHP regex, plus tests for the
anonymous reuse (`cite_ref-1` then `cite_ref-1-1`) and the normalization. The
scheme now matches the service id-for-id on the page; the *numbers* still drift
(rustoid collects 528 notes against the service's 545), so a collection gap
remains, and `2024 Summer Olympics`'s first difference is upstream of all of it —
an `about="#mwt10"` where rustoid has `#mwt8`, i.e. the document-order `about`
counter, not Cite.

### The `cite_*` id number is global, the label is per group

That "collection gap" was mostly a second id bug. Cite's `RefGroupItem` carries
two counters: `numberInGroup` (per group, drives the `[1]`/`[a]` label) and
`globalId` (one sequence across every group, drives the `cite_*` ids). rustoid
has one `number` doing both, so on a page with `{{efn}}` notes every grouped
ref got the wrong id: `cite_note-ANI_medal_table_inclusion-1` where the service
has `-187`, and every subsequent id shifted.

`Reference` now carries `global_id` beside `number`; the ids read `global_id`,
the labels still read `number`. On `2024 Summer Olympics` the id sets went from
528/275 to 534/281 matched notes/markers, with 518 ids common (from far fewer).

What is left is **not** a collection gap but an under-rendered transclusion: the
10 remaining notes (`WW1`, `WW2`, `COVID2021` in `Template:Olympic Games`'s
navbox; `Who_is_INA`, `ANI_medal_table_inclusion` in the transcluded
`{{:2024 Summer Olympics medal table}}`; and anons `189`/`191`/`257`) sit in
subtrees rustoid renders incompletely — its navbox carries an unresolved
`aria-labelledby="[[File:…]]_[[Olympic_Games]]8788"`, its medal table has the
same row count but a third of the `INA` cells. Fixing those is a
`Module:Navbox`/expansion investigation, not a Cite one.

### The `about` drift is a `<ref>` body that is never expanded

`2024 Summer Olympics`'s first difference at 8910 is `about="#mwt10"` (service)
vs `#mwt8` (rustoid) on the first ref marker. The gap is not in Cite: it is that
**rustoid never expands a `<ref>`'s body**. A `<ref>`'s content is opaque to the
tokenizer, and the port renders it from the raw wikitext at the note list
without running the template pipeline, so:

- the note shows the literal token — 252 `<template cite …="">` elements leak
  into the page where the service has `<cite class="citation">` (5 vs 257);
- the citation's own templates never take their `about` ids, and the ids they do
  take (templatestyles, the `cite` wrapper) are spent when Cite renders the list
  — after every other id — instead of at the ref site, where the service spends
  them (its note-1 content is `#mwt9`, *before* the marker's `#mwt10`).

Two placements were tried and both were **reverted**:

- expand every body before Cite's render (walk the DOM, expand each distinct
  body through `expand_templates`). Content is fixed — 0 leaked `<template>`, 253
  citations, `2024 Summer Olympics` grows 1.28 MB → 1.72 MB against the service's
  1.90 MB — but the ids are still spent after the main expansion, so ref1's
  marker stays `#mwt8`, and the extra ~1000 template expansions make the
  ref-heavy `France`/`India`/`Israel` exceed the stall cap (they render nothing in
  120 s).
- expand at the ref site, inside `expand_templates`' walk (the service's order).
  Same stall, and the marker ids still did not move, because a template argument's
  extensions are numbered where the value lands, not where it expands.

The correct fix — expand the content at the ref site — is therefore not enough
on its own: rustoid's expansion is too slow to afford ~1000 more template
expansions on a ref-heavy page, and the first difference does not move until the
ids interleave exactly. Landing it needs the expansion made cheap (a per-page
share of whatever the service caches) first. Recorded rather than shipped, and
the working tree is back to the three commits above.

## The navbox `above` list, and the `Nl`-trim it exposed

The navbox investigation from the previous section's last paragraph (rustoid's
`Template:Olympic Games` navbox lost list structure) reduced to a one-line
reproduction against the live transform endpoint:

```
{{Navbox
| name = Olympic Games
| title = [[File:Olympic rings without rims.svg|30px]] [[Olympic Games]]
| above =
* '''[[Olympic sports]]'''
* '''[[Olympism]]'''
}}
```

Rustoid emitted `<li><b>Olympic sports</b>* <b>Olympism</b></li>` where the
service emits two `<li>`s. The `* ` is literal, so the value `Module:Navbox`
received from `frame.args.above` had lost the newline between the items.

### The two bugs, and the wrong turn between them

The value is expanded by `expand_invoke_arg_text` → `argument_value_text`, which
covered `Str`, `mw-quote`, `{{!}}`, wikilinks, tags and entities — and **dropped
`Nl` tokens**, with a comment claiming `tokensToString` drops them. That is true
in *attribute* context, but a module's argument is expanded wikitext and
MediaWiki's preprocessor keeps the line break. Rendering `Nl` as `\n` fixed the
navbox (and every navbox `above`/`below`/list).

It also moved `2024 Summer Olympics`'s first difference *earlier*, from 8910 to
**2077 (0.11%)** — a regression, on `{{Use British English}}`. That template is
`{{safesubst:<noinclude />#invoke:Unsubst||…|$B=\n{{DMCA|Use British English
…}}}}`, and `Module:Unsubst` returns `$B`. The newlines leaked into a category
name: `[[Category:Use British English\n from\n July 2024\n]]` stopped parsing as
a link and rendered as literal text, and the two adjacent transclusions merged.

### The real cause: a named argument is trimmed, newline tokens included

The category's newlines came from `Template:Dated maintenance category
(articles)`, whose body passes `|1={{{1|}}}` — one argument per line, so each
value ends in `\n `. MediaWiki trims a named argument's value **after
expansion** (`PPTemplateFrame_Hash::getNamedArgument` runs `trim()`; a truly
positional argument is not trimmed). Rustoid models that in `Frame`'s argument
lookup, but `trim_items` only trimmed the outermost `Item::Str`. A value like
`|1=A\n` expands to `[Str("A"), Nl]`, so the trailing newline token survived the
trim — invisible while `tokens_to_string` dropped it, fatal once
`argument_value_text` kept it and spliced it into a link target.

`trim_items` now drops edge `Nl` tokens and whitespace-only `Str`s as well.

### Effect

- `2024 Summer Olympics`'s first difference returns to **8910 (0.47% of
  parsoid)**, unchanged from before — the navbox is far past it — while rustoid's
  output grows 1 282 656 → 1 288 905 bytes: the lists are real now.
- The `{{Use British English}}`/`{{Use dmy dates}}` categories are byte-identical
  again (`Category:Use_British_English_from_July_2024`), and the two transclusions
  no longer merge.
- Fixture guard **877/896**, workspace tests green. `trim_items` has a unit test
  that asserts the edge `Nl` tokens are gone.

## The navbox `Template loop detected`, and the stale `src` under it

After the `above` list fix the Olympic Games navbox still diverged structurally:
**13 of the service's 16 `navbox-subgroup` tables**, 50 of 63 `navbox-group`
cells, and `INA` 3 against 20 — the medal table's rows are among the missing
output. The cause reduced to one line against the transform endpoint:

```
{{Navbox
| name = Test
| title = Test
| list1 = {{Navbox|child
  | group1 = G
  | list1 = * A
  }}
}}
```

Rustoid renders `<span class="error">Template loop detected: Template:Navbox</span>`
where the service renders the nested navbox. The service's `Module:Navbox` is
*not* looping: rustoid is.

### The chain, and the stale `src` that builds it

Tracing the expansion (`RUSTOID_DBG_FRAME`) gives it exactly. The `list1` value
is expanded — correctly, in the caller's frame — to real HTML; its tokens are
`div`/`table`/`tr`/`th`/`td`/`listItem`/`wikilink`, and `expandable_content`
finds no template in them, so `expand_invoke_args` leaves the value alone. The
text the module then receives is built by `argument_value_text`, whose live-tag
arm reads each token's `data_parsoid.src` — and for these expanded tokens that
`src` is the **stale template call**, `{{Navbox|child…}}`. So `frame.args.list1`
is the raw call, `Module:Navbox` echoes it with `:wikitext()`, and the `#invoke`
output re-expansion meets `{{Navbox}}` again in a frame whose chain is
`["Template:Navbox", "Template:Navbox", "Sandbox"]` (the output runs in a child
of the `Template:Navbox` frame) — a genuine-looking loop, replaced by the error
span.

So the navbox gap is the *same* missing piece the docs already name for the
`about` drift: a faithful token→**expanded-wikitext** serializer. `argument_value_text`
covers every token shape for values whose `src` is the written text, but an
already-expanded value needs its HTML/wikitext rebuilt from the tokens
(each tag from its attributes), not read back from a `src` that still holds the
call that produced it. `{{Main other|{{Main other|X}}}}` works because that
value goes through `{{{1}}}` substitution, which splices the tokens directly;
the `#invoke` path renders to text first, and that is where the `src` is stale.

### Reverted, not shipped

`expand_invoke`'s `frame:getParent().args` are expanded in the invoke frame;
MediaWiki expands them in the *caller* (`PPTemplateFrame_Hash::getNamedArgument`
uses `$this->parent`), so a first attempt routed `ArgSource::Parent` slots
through `frame.parent()`. It changed no output here — because the values arrive
already expanded, `expand_invoke_args` skips them — so it was reverted rather
than shipped as a no-op, and the node reverted with it. The correct fix is the
expanded-value serializer above; recorded so the next session starts at it.

Effect of the `above`/trim commits on this page, for the record: rustoid
1 282 656 → 1 288 905 bytes, first difference unchanged at **byte 8910
(0.47% of parsoid)**, corpus output 30 492 786 → 30 557 367 bytes at the same
0.61x, still 0/41 pages passing.

## The expanded-value renderer: `listItem` and the self-closing tokens

The section above named the navbox loop's prerequisite — a faithful renderer for a
module argument whose tokens came out of *expansion* — and left it open. It turned
out to be two missing arms in `argument_value_text`, not a general serializer.

`RUSTOID_DBG_ARG` on the smallest reproduction
(`{{Navbox|…|list1={{Navbox|child|…}}}}`) showed the value declining on exactly
one token: a `listItem` with no `src`. A list item's marker lives in its `bullets`
attribute — `tokens_to_string` already renders it that way — and a value that
holds a list is otherwise plain HTML. So the first arm reads `bullets`. That alone
fixed the nested navbox: loop 0, `navbox-subgroup` 6 = 6, and on
`2024 Summer Olympics` the navbox groups came back (50 → 59 of 63).

A second `RUSTOID_DBG_ARG` pass over the page then found the only remaining
declines: self-closing tokens that *do* have a `src` — a bare `urllink` (a
citation `url`), an `extlink`, a `<br>`, a behavior switch. Each reaches the module
as its own source, so the renderer now emits `src` for any self-closing token it
has no special arm for. That fixed a wrong citation URL on the page (the `url`
argument had fallen back to a stale source range and `Module:Citation` rendered
`…/olympic-games/` instead of `…/olympic-games/paris-2024`) and removed a
`CITEREF_temp_preview…` id.

### Effect

`2024 Summer Olympics` 1 288 905 → **1 291 183 bytes**, first difference unchanged
at byte 8910 (0.47% of parsoid). Corpus output 30 557 367 → **31 214 541 bytes**.
No page's *first* difference moved — the navbox and the citation are both past it
— and none regressed; three pages shrank by 4–557 bytes, which is the citation fix
removing an unexpanded span rather than a loss. Guard **877/896**, all unit tests
green (the renderer has a test for each new arm).

### What is left in the navbox: a templatestyles element in an argument

With those two arms, the nested `{{Navbox|child}}`s that still loop are the ones
whose value holds a `<style>` element with no `src` — the templatestyles
`Module:Navbox` emits through `frame:extensionTag`. Rustoid has already
*substituted* the strip marker into that `<style>` by the time the outer module
reads its argument, so the renderer declines and the value falls back to the raw
call. `Module:Navbox` strips templatestyles markers from its arguments and sums
the *remaining* lengths into `args.argHash`; a resolved `<style>` neither strips
nor matches, which is why the id suffix reads `8789` where the service reads
`12164`. The faithful shape is to keep the marker in the argument and let the
module strip it — the templatestyles post-pass the earlier sections keep pointing
at — not to render the `<style>` here. Recorded rather than hacked around, since a
`<style>` emitted here would diverge in exactly the opposite direction.

## Offline first differences are half fact drift; `Zebro` after the facts

The offline corpus is a pessimistic, and *misleading*, instrument for finding
parser bugs. `get_page_info` — link existence, `mw-redirect`, `mw-disambig` — is
answered by the harness's `prop=info` and cached under `PageInfo`, but a title no
online run has looked up is guessed offline as `existing()`: the page exists, the
link is blue, and **no link class** is emitted. Parsoid's oracle, rendered when
the facts were live, does emit `class="mw-redirect"`.

`Zebro`'s first difference was exactly that: `[[Asiatic wild ass]]` hotlinked in
the service (`class="mw-redirect"`) and plain in rustoid. It is **not a parser
bug**. One online run of the page (`rustoid-compare --page Zebro` with no
`--offline`) populated `info:Asiatic wild ass` and the other targets, and the
first difference moved **9294 → 11052 (6.79% → 8.07%)**:

```
<sup about="#mwt16" …>   rustoid
<sup about="#mwt10" …>   parsoid
```

So the *real* first difference on this small page is the same one
`2024 Summer Olympics` has at 8910: the extension id the ref marker gets. The
`about="#mwtN"` sequence confirms it — both agree up to `#mwt6`, then parsoid
spends `8,9` on the first ref body's extensions before numbering the marker `10`,
while rustoid numbers the marker `16`. That is the extension-numbering phase
(§"The extension-numbering phase"), from the ref-body side: rustoid never expands
a `<ref>` body at the ref site, so the ids it *does* spend there fall in a
different order.

Practical consequence for the methodology: a page whose first difference is a
link class has not been tested at all. Running it online once to warm `PageInfo`
(and then comparing offline, as above) is what makes its scoreboard entry
meaningful. The alternative — populating the whole corpus online — is what the
429 rule forbids, so this is a per-page step.

### `Help:Introduction`'s first difference: a stray empty nowiki

The smallest page fails at 5161 on the `{{pp-semi-indef}}` protection indicator.
rustoid emits an extra attribute-less nowiki span before the indicator meta:

```
…id="mwBA"><span typeof="mw:Nowiki"></span><meta typeof="mw:Extension/indicator" …>   rustoid
…id="mwBA"><meta typeof="mw:Extension/indicator" about="#mwt3" …>                    parsoid
```

It is not the `<noinclude>` handling — a direct probe of
`A<noinclude>{{Pp-semi-indef|small=yes}}</noinclude>B` matches the service exactly,
`<span typeof="mw:Nowiki mw:Transclusion" about="#mwt1">…</span>` and all. On the
page, the pp transclusion is *merged into the enclosing transclusion's `parts`*
(the `<p about="#mwt4">`'s `data-mw` lists `Pp-semi-indef` as part `i:0` and the
literal `</noinclude>` as its own part), and in that state Parsoid drops the
redirect's nowiki wrapper while rustoid keeps an empty one — and the meta loses
its `about`. So the bug is in the interaction between the redirect-nowiki wrapper
and the transclusion merge, not in include limits. Recorded, not reduced further.

## The `<ref>` body is expanded at the ref site (and TT2 is chunked per line)

The two earlier attempts at this were reverted because expansion was three times
more expensive then, and a body expanded at the wrong *place* moves the whole
`about` sequence. This time the placement is right, and the improvement is
measurable. Two changes, and the second is the one that actually fixed the ids.

### The body expansion

`Parser::expand_templates` already resolves `<templatestyles>` and `<indicator>`
inline because their ids belong in the chunk. A `<ref>` is the same shape, one
level down: PHP's `ExtensionHandler::onExtension` calls
`RefTagHandler::sourceToDom` → `extTagToDOM`, which parses the body *in the
current frame* (spending the ids of any extension inside it), and only then does
`onDocumentFragment` allocate the wrapper's `about`. Cite's DOM pass never
re-parses the body; it *moves* the already-rendered nodes.

rustoid keeps Cite as a late DOM pass, so `number_extension_token` now renders a
`<ref>` body there and stashes the node on the parser under the about id it just
allocated (`Parser::ref_bodies`). Cite's list renderer looks the body up by that
key (`Reference::body_about`) and falls back to the synchronous inline render
only when there is no stashed node — a body reached through a deferred argument,
or one built outside expansion. The rendered note now carries the citation HTML
(`{{cite journal}}` → `<cite class="citation journal cs1">`) instead of a bare
`<template>` element: `Zebro`'s first note went from
`<span …><template Cite book …></template></span>` to the real citation, and the
page grew 90 801 → 125 819 bytes against parsoid's 138 950.

A body that cannot spend an id of its own — no `{{` and no `<` — is *not*
pre-expanded: Cite's fallback renders it identically and leaves the id sequence
untouched, which keeps the plain-text and bare-URL notes cheap. On `France`,
whose refs are nearly all citation templates, this changes nothing.

### The chunk boundary is what moved the marker

The body expansion alone did not fix the marker id; it moved it the wrong way
(`#mwt16` → `#mwt18`). The reason is that `expand_templates` treated the whole
page as one TT2 chunk, so the end-of-chunk extension numbering ran after *every*
template on the page. PHP does not: `ParserPipelineFactory::parse` calls
`parseChunkily`, and `PegTokenizer::processChunkily` yields **one top-level block
per chunk** — one line. Each chunk runs `TemplateHandler` and then
`ExtensionHandler` over itself, so the id order resets at every line. rustoid's
flat token stream carries that boundary on each top-level `Nl`, so
`expand_templates` now splits its input at `Nl` tokens and runs the walk and the
extension post-pass per chunk, carrying `table_depth` across (a table spans
lines).

The rule is pinned by the transform endpoint:

```
<ref>a</ref>{{1x|b}}        → template #mwt1, ref #mwt2   (one chunk)
<ref>a</ref>\n{{1x|b}}      → ref #mwt1, template #mwt2   (two chunks)
<ref name=q>a</ref>{{1x|b}}\n{{1x|c}}
                            → template #mwt1, ref #mwt2, template #mwt3
```

With that, `Zebro`'s marker is `#mwt10`, matching the service, and its first
difference moved **11 052 → 11 493 (8.07% → 8.40% of parsoid)**. The whole
`about` sequence past the marker now agrees. The unit test
`extensions_are_numbered_after_templates_in_a_chunk` still passes unchanged — the
same-line case it pins is exactly what the chunk model preserves — and gained an
assertion for the across-line reset.

### The new first difference is the TOC meta

At 11 493 rustoid omits `<meta property="mw:PageProp/toc"
data-mw='{"autoGenerated":true}'>`, which parsoid emits at the end of section 0
before the first heading. `rustoid` has no TOC handling at all; the meta is not
in Parsoid's own DOM processors (it is core's `Parser::TOC_PLACEHOLDER`, added in
integrated mode), so it is a new feature rather than a `about`-id drift. It is
the current first difference on `Zebro` and is the next thing to build.

### Cost

Expanding every citation-bearing `<ref>` body is the expensive half. Offline:
`France` now renders in **4m0s** where it previously completed inside the corpus
cap, and a 24-page half-corpus run at `RUSTOID_PAGE_STALL_SECS=200` now stalls
three pages (`France`, `Canada`, `India`) rather than one. The correctness is not
in question — PHP does the same work — but rustoid's expansion is the bottleneck,
and the cap now bites on the largest articles. Recorded rather than papered over;
the honest reading of a `stalled` line is "correct work, too slow", not "wrong
output".

Corpus (offline, 24-page half): **0/20 passing**, 3 stalled, every completed
page's difference tagged `suspect drift` (the baseline facts are days older than
the oracles — see §"The age guard"). No page's first difference regressed.

## The auto TOC meta, and an id that follows `isEmpty`

Two more differences past the ref-body fix, both `id`-sequence issues, both now
fixed on `Zebro`.

### Core's auto-generated TOC placeholder

At 11 493 rustoid omitted `<meta property="mw:PageProp/toc"
data-mw='{"autoGenerated":true}' id="mwGA"/>`, which parsoid emits as the last
child of section 0 (right before `</section>`). It is not Parsoid's: Parsoid's
`BehaviorSwitchHandler` only turns an authored `__TOC__` into
`<meta property="mw:PageProp/toc">` (bare), and the auto placeholder is core's
`Parser::TOC_PLACEHOLDER`, appended to section 0 of a main-namespace page with at
least four headings and no `__NOTOC__`/`__TOC__`.

The corpus cache made the two shapes easy to tell apart: the majority of main-ns
pages carry the meta with `data-mw` and an `id`; the pages built around
`{{TOC limit}}` carry a *bare* one inside the template's own
`<div class="toclimit-N">`, because `Template:TOC limit` is
`<div class="toclimit-…">__TOC__</div>` and the behavior switch — not core —
produced the meta. `pipeline::auto_toc::insert` counts the headings (from four
up), bails on an authored `mw:PageProp/toc` or `notoc`, and appends to the lead
section with the `data-mw` that gives it a generated id. It is a no-op wherever
section wrapping did not run, so the fixture suite cannot see it — which is
exactly why the 896-fixture guard has never caught the missing meta.

`Zebro`: 11 493 → **16 652** (8.40% → 12.17%).

### The media `<img>` id

At 16 652 every thumbnailed image was one id short: parsoid's `<img>` carried
`id="mwSQ"` and rustoid's none, so every id after it shifted. `assign_node_ids`
gave a node an id only when its `data-parsoid` held a `dsr`/`tsr`, but Parsoid's
rule is `!$dp->isEmpty()` — and `DataParsoid::isEmpty` is false as soon as *any*
serializable field is present. A media `<img>` is the case that exposes it: the
transform endpoint shows
`data-parsoid='{"a":{"resource":…},"sa":{"resource":…}}'` with no `dsr`, and
parsoid stamps it an id. `add_media_info` now serializes the img's shadow info
into `data-parsoid` (which the transform endpoint shows it carries anyway), and
the rule draws an id from any non-empty object bar a `tmp`-only one. Only `tmp`
is discounted, as Parsoid discounts it — it is transient and never serialized.

This one rule change touches every page's ids, so it was checked on the
nine-page subset rather than trusted. Against the parsoid totals:

```
Bicycle                  11836   (unchanged)
Zebra                     2172   (unchanged)
Help:Introduction         5161   (unchanged)
Unix                      4772   (unchanged)
Nobel Prize        1112 -> 5926
Megadeth           2801 -> 7463
Quicksilver (film) 2732 -> 4213
Sundial            1897 -> 3934
List of sovereign… 4009 -> 8409
```

No page regressed. `Zebro`: 16 652 → **23 570** (12.17% → 17.22%).

### The next Zebro difference is architectural

At 23 570 the id of a transclusion *inside a media caption* differs: parsoid
numbers it during the media token's TT2 handling (`#mwt36`), rustoid `#mwt119`.
The cause is the same class as the ref-body one, one pass further out: rustoid
runs `render_links`/`render_file` as a single pass *after* `expand_templates`,
where Parsoid runs `WikiLinkHandler` as part of TT2 — per chunk, after that
chunk's `TemplateHandler`. So a caption's nested expansion lands after the whole
page's templates here rather than after its own chunk's. Moving the link/media
pass into the per-chunk flow is a larger refactor than the ref-body fix was;
recorded, not attempted.

## A body's attribute values are expanded with `inTemplate`
At 2172 on `Zebro`'s sibling `Zebra`, and independently at 5161 on
`Help:Introduction`, the id sequence is short a spurious expansion. The cause
turned out to be a rule rustoid was missing, exposed by an experiment that was
reverted.

### The reverted experiment: interleaving `expand_attributes` per chunk
The previous session left an uncommitted change that split `build_ast` into
per-line chunks and ran `expand_templates → expand_in_attributes →
expand_attributes` inside the loop, mirroring Parsoid's
`TokenHandlerPipeline::processChunk`. It was never compiled or tested. Compiling
and running it **regressed `Zebra` from 2172 to 621**: parsoid's `distinguish`
transclusion is `#mwt2`, rustoid's `#mwt3`. One `about` id was spent between the
`{{Short description}}` wrapper (`#mwt1`) and `distinguish` — and the id was
never emitted, so the trace (`RUSTOID_TRACE_ABOUT`) was the only way to see it.

`RUSTOID_TRACE_ABOUT=bt` named the allocator: chunk 1's `expand_attributes`
itself, via `expand_templates → expand_template_token`. The chunk-1 token is the
short-description output, and its `[[Category:{{{pagetype|{{pagetype…}}}}} with
short description]]` is a wikilink whose `href` is a *token array* (the
`{{{pagetype}}}` default is a template). `expand_attributes` expanded that array
with `in_template = false`, so the nested `{{pagetype}}` took an `about` id.
Moving `expand_attributes` into the loop only moved that id earlier; it did not
create it. The change was reverted uncommitted.

### The rule
Parsoid's `AttributeExpander::buildExpandedAttrs` passes
`$wrapTemplates = !$this->options['inTemplate']` to `stripMetaTags`, and the
`mw:ExpandedAttrs` `about` is added only when a meta in the value set
`hasGeneratedContent` — which `stripMetaTags` does only under `$wrapTemplates`.
So when the token belongs to a **template body** (`inTemplate = true`), an
attribute value that expands to a transclusion gets **no** `about` and **no**
`mw:ExpandedAttrs`. rustoid already suppresses the *marking* for such tokens
(`TempData::synthesized`, `build_expanded_attrs`), but it still ran the nested
`expand_templates` with `in_template = false`, so the expansion inside the value
was wrapped and spent an id anyway.

`expand_attributes` now reads `synthesized` off the token and expands that
token's key/value arrays with `in_template = true` — the body-pipeline value — so a template reached through a body's attribute is unwrapped. On
`{{Short description|…}}` the trace drops from `#mwt1, #mwt2` to `#mwt1`; the
rendered category link is byte-identical either way.

This is the prerequisite the reverted experiment needed: the spurious id is what
made interleaving look like a regression. Whether the interleave is worth
finishing (the caption id at 23 570 needs `render_links` moved per chunk, not
`expand_attributes`) is the next question, but it is now separable.

### Effect
No measured first difference moves: the id was spent late (after every
top-level template) and was swallowed by `stripMetaTags` before rendering, so it
only ever shifted ids *after* it. `Zebra` 2172, `Bicycle` 11836,
`Help:Introduction` 5161, `Nobel Prize` 5926, `Sundial` 3934, `List of
sovereign states` 8409, `Quicksilver (film)` 4213 — all unchanged. The fixture
guard holds at 877/896 (it cannot see ids), and the whole workspace is green.
Recorded here rather than shipped as a no-op: it is a real id drift, and it is
the difference between the per-chunk experiment regressing and not.

### The interleave, finished for `AttributeExpander` only
With the synthesized-attribute rule in place, splitting `build_ast` into per-line
chunks and running `TemplateHandler → ExtensionHandler → AttributeExpander`
inside the loop no longer regresses. It is also where the caption id actually
comes from: a media caption is the link token's `mw:maybeContent` KV, and
`expand_attributes` — not `render_links` — expands a KV's token array. So the
one pass that had to move was `expand_attributes`; the caption's nested
transclusion is now numbered in its own chunk and lands at parsoid's `#mwt36`.

Moving `render_links` into the loop too (as the doc above proposed) **drops
`{{citation}}`'s CS1 `<style>`**: the stylesheet placeholder is a top-level
`<style typeof="mw:DOMFragment">` token, and a per-chunk `render_links` reached
it before the fragment it names had been resolved, so the first citation emitted
no style and the second — the only survivor — became a full `<style>` instead of
a `mw-deduplicated-inline-style` link. The first-difference metric did not see
it (the drop is at 123 952, past the 24 039 difference), which is exactly why it
was checked by counting: `31` templatestyles / `25` dedup links in parsoid,
`5`/`1` in rustoid both before and after, but `4`/`0` with `render_links`
interleaved. `render_links` stays a global pass. Recorded because the metric
alone would have shipped it.

### Effect
`Zebro`: 23 570 → **24 039** (17.22% → 17.56%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409), the fixture guard holds at 877/896, and the workspace
is green. The next `Zebro` difference is a `<style data-mw-deduplicate>` for
`Plainlist/styles.css` that parsoid emits inside the transclusion span and
rustoid omits — the same missing-in-content-styles gap (31 vs 5) that the
citation case exposed, now the leading one.

## A caption's `<templatestyles>` is resolved against the document fragment space
The difference the interleave exposed is now gone: rustoid emitted a
`mw:Transclusion` span with *no* `<style>` where parsoid had the
`Plainlist/styles.css` (and `Legend/styles.css`) sheet. Reduced to
`[[File:Example.jpg|thumb|{{#invoke:list|unbulleted|a|b}}]]`: rustoid's
`<figcaption>` carried the `plainlist` `<div>` but not the sheet.

The sheet is resolved while the caption's `mw:maybeContent` value is expanded
(`expand_attributes`), and `number_style_placeholders` stashes it in
`self.ext_fragments`. But a caption is rendered by its own sub-pipeline, and that
pipeline was handed a **fresh** fragment map and a **fresh** id counter:

```
&mut |items| { let mut f = HashMap::new(); let id = Cell::new(0); … }
```

so the placeholder's `data-fragment-id` (a slot in the document-wide
`Env::newFragmentId` space) named nothing and the `<style>` vanished. The
`CaptionFragmentBuilder` now takes the map and counter as arguments, and every
call site passes the real ones, so a caption resolves the same fragments as the
page. `WikiLinkHandler` likewise renders a caption through `PipelineUtils`, never
a private fragment space.

The other half is *when* the stash becomes visible: `build_ast` folded
`ext_fragments` into `fragments` only at the very end, after `render_links` had
already rendered the captions. The fold now happens once, right before
`render_links`. Doing the fold *per token* inside `expand_attributes` instead
**dropped every hatnote `<style>`** (Zebra 2172 → 795): a value rendered there
resolved the page's style fragments into its own `data-mw` html, orphaning the
placeholders that still needed them at tree-build time. One fold, after the
chunk loop, has neither problem.

### Effect
`Zebro`: 24 039 → **25 089** (17.56% → 18.33%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409, `Unix` 4772), the fixture guard holds at 877/896, and
the workspace is green.

## The caption's nested expansion is now attribute-expanded
At 25 089 the difference was `Template:Legend` inside the media caption: parsoid's
`<span class="legend-color mw-no-invert" style="background-color:#188fad;
color:black;…">`, rustoid's bare `<span class="legend-color " style="…">`. Both
missing pieces come from the template's *attribute* values —
`class="legend-color {{#if:{{{invert|}}}|skin-invert|mw-no-invert}}"` and
`style="…{{greater color contrast ratio|{{{1}}}|white|black|css=y}}…"`.

Reduced:
```
[[File:Example.jpg|thumb|{{legend|#188fad|X}}]]   rustoid 0 no-invert / 0 color, parsoid 1/1
{{unbulleted list|{{legend|#188fad|X}}}}          both 1/1  (a page-level legend)
```

So the difference was the *context*, not `Template:Legend`: the caption is the
link token's `mw:maybeContent` KV, expanded by `expand_attributes` →
`expand_templates`. That nested expansion produced `Template:Legend`'s
`<span>` — whose `class`/`style` are themselves templated — but nothing ran the
`AttributeExpander` over the nested output, so those attribute templates stayed
literal. On the page level the span is produced by the *outer*
`expand_templates` and the per-chunk `expand_attributes` then reaches it, which
is why `{{unbulleted list|…}}` matched.

Parsoid has no gap because `AttributeTransformManager::process` expands a value
with `Frame::expand(… 'attrExpansion' => true …)` and `buildExpandedAttrs`'s
`expandAttrValuesToDOM` runs the value through a pipeline that includes its own
`AttributeExpander`.

### The fix
`expand_attributes` is split: the loop lives in `expand_attributes_at(…, depth)`,
and each key/value goes through `expand_attr_tokens` — `expand_templates`, then
`expand_in_attributes`, then (bounded by `MAX_ATTR_EXPANSION_DEPTH`) a recursive
`expand_attributes_at` over the expansion, but only when `has_token_attributes`
says one of its tokens actually carries a token-array attribute, so a value that
collapses to plain strings pays nothing. `build_expanded_attrs`' value renderer
became `&mut dyn FnMut` so the fragment map threads through without the old
`RefCell` dance.

### Effect
`Zebro`: 25 089 → **37 308** (18.33% → 27.26%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409, `Unix` 4772), the fixture guard holds at 877/896, and
the workspace is green.

### The next `Zebro` difference: a `<ref>` in a caption is numbered late
At 37 308 the difference is the `about` of a `<ref>` inside the Altamira image
caption: parsoid `#mwt68`, rustoid `#mwt130`. The reduced form
`A [[File:Example.jpg|thumb|caption<ref>Note text</ref>]] B` is off by exactly
one (parsoid `#mwt1`, rustoid `#mwt2`), so the minimal probe does not reproduce
the magnitude — the page case is about *when* the caption is numbered relative to
the surrounding chunks, not a fixed offset. Parsoid numbers the ref when the
media token is handled in its own chunk; rustoid's id is 62 later, i.e. after
later chunks. Worth checking whether the caption's `mw:maybeContent` is still
reached by a pass that runs after the chunk loop (`render_file` expands a caption
too), rather than by the in-chunk `expand_attributes` the previous fix relied on.
Recorded for the next session.

### A caption is only re-tokenized for a `<nowiki>`, not for any extension
The `<ref>` in the Altamira caption is now `#mwt1` in both. `render_file`'s
block path runs `tokenize_caption_items` over the already-expanded caption, and
that function had one rule for two jobs: it re-tokenized the caption from source
whenever *any* `language-variant` or `extension` token was present. For a
`language-variant` (or a `<nowiki>`) that is needed — those can split a
surrounding `[[…]]` across items, and their `data-parsoid.src` reassembles it.
For a `<ref>` it is destructive: the caption holds the *expanded* ref (with the
`about` id the extension handler spent on it and the body stashed for Cite), and
reconstructing `caption<ref>Note text</ref>` from source replaced it with a fresh
unexpanded ref. Cite then fell back to its own counter (`take_about`) and
numbered the ref late — the `#mwt130`/`#mwt68` gap, and the single-id gap
(`#mwt2`/`#mwt1`) that the minimal `A [[File:…|thumb|caption<ref>…</ref>]] B`
showed. The rule now reconstructs only for `language-variant` and for an
`extension` whose `name` is `nowiki`; the per-chunk path keeps every other token,
as Parsoid's block caption does (`getDOMFragmentToken` on the tokens, no
re-tokenization).

### Effect
`Zebro`: 37 308 → **59 383** (27.26% → 43.39%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409, `Unix` 4772), the fixture guard holds at 877/896, and
the workspace is green.

## The tree stage restarted the fragment ids at a taken value
The next difference was a wikilink that rendered as its own link trail: parsoid's
`called <a …>onagers</a>` (from `[[onager]]s`), rustoid's
`called <a …>s</a>` — the link text gone, only the moved trail left. It reduced to
nothing on its own; the page needed the surrounding content, which is the tell
that it was a fragment-**id collision**.

`TreeBuilderStage::to_ast_with_fragments` starts its fragment counter at
`fragments.len()` so a `<nowiki>` expanded *during tree building* continues after
the pre-built fragments. But the fragment ids come from several passes and have
gaps, so `len()` is not the next free id. On `Zebro` the map held 114 entries with
ids up to 117; the counter restarted at 114, and a `<nowiki>`'s
`<span typeof="mw:DOMFragment" data-fragment-id=114>` took the slot the second
`[[onager]]`'s link-text fragment already occupied. The tree builder resolves a
start-tag-tunnel (the nowiki span) before the self-closing placeholder, so the
span consumed the wikilink's fragment and the placeholder then found nothing:
`Node::document()` fallback, an empty `<a>`, and the trail `s` appended on its own.

The trace that pinned it: `insert fragment id=114 token=wikilink` (the link text,
from `dom_fragment_token`), then `nowiki_fragment_items id=114 cell=0x…e80`
(a *different* `Cell`, the tree stage's), then
`start_tag remove fragment id=114 name=span` and
`resolve dom-fragment id=Some(114) present=false`.

The counter now starts one past the highest key (`next_fragment_id`), so it can
never name a pre-built fragment. (Parsoid has no such bug because `Env`
allocates every fragment id from one `newFragmentId` counter; rustoid's tree
stage runs after the others and only needs not to collide.)

### Effect
`Zebro`: 59 383 → **62 583** (43.39% → 45.72%), with `[[onager]]s` correct.
Every other page's first difference is unchanged (`Zebra` 2172, `Bicycle` 11836,
`Help:Introduction` 5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver
(film)` 4213, `List of sovereign states` 8409, `Unix` 4772; the byte totals move
slightly because a `<nowiki>` fragment no longer replaces a link), the fixture
guard holds at 877/896, and the workspace is green.

## The next `Zebro` difference: a `<references>` from `#tag` is numbered before the reflist's `<templatestyles>`
At 62 583 the difference is the `about` of `Template:Reflist`'s stylesheet:
parsoid `#mwt123` on the `<style>`, `#mwt124` on the `<references>`; rustoid the
reverse (`#mwt123` references, `#mwt124` style). `Template:Reflist` is
`<templatestyles src="Reflist/styles.css"/>…{{#tag:references|…}}`, so document
order is style first and parsoid numbers them in that order.

The cause is the order in which the two ids are taken. The style is resolved
during the expansion walk but its id is deferred to the chunk's post-pass
(`number_style_placeholders`, "templates take ids, then extensions in source
order" — the rule the transform endpoint pinned with
`<templatestyles/>{{Center|b}}`). The `<references>`, however, is produced by
`#tag:references` *inside* a parser-function branch, and its nested expansion's
own post-pass numbers it immediately. So it takes its id before the outer chunk
reaches its style pass, and the two come out swapped.

### The attempted fix, reverted
Interleaving the two in the chunk post-pass — one source-order loop numbering a
style placeholder or an extension per item — reads as the faithful fix, and was
tried. It is a no-op here: the `number-order` trace (`RUSTOID_TRACE_NUMORDER`,
removed) shows the `<references>` already carrying `about="#mwt123"` when the
`style(frag=32)` item reaches the pass, because a *nested* expansion numbered
it. The real shape is that an extension a nested expansion produces should be
numbered in the outer chunk's post-pass, at its document position relative to
the styles and other extensions — the same deferral `style_defer` already
applies to extensions inside a template argument. That is a larger change and
is left for the next session; the interleave was reverted rather than shipped as
a no-op.

## Reflist: a branch now defers its extensions, and styles interleave
The difference at 62 583 is fixed, and it needed both halves, which is why the
earlier interleave on its own was a no-op.

`Template:Reflist` is `<templatestyles src="Reflist/styles.css"/>…{{#tag:references|…}}`.
Two things put the ids in the wrong order:

1. The `<references>` comes from `#tag:references` inside an `#if`/`#switch`
   branch. rustoid re-expands a parser-function branch with its own
   `expand_templates`, whose post-pass numbered the extension *then* — before
   the reflist body's own post-pass reached the `<templatestyles>`. The branch
   is spliced into the enclosing chunk, so its extensions belong to that chunk's
   source-order pass. The branch expansion is now wrapped in
   `begin_style_defer()` (the same deferral `style_defer` already provides for a
   template argument's value), and the frame is unchanged — a parser function
   expands its branch in the caller's frame — so nothing is lost.

2. With both in one chunk's post-pass, the pass still ran every extension before
   every style, so source order did not decide. `number_style_placeholders` is
   split into a per-item `number_style_placeholder`, and the post-pass is now one
   loop that numbers a style placeholder or an extension per item, in document
   order:

   ```
   for item in out.iter_mut() {
       if self.number_style_placeholder(item, about_counter) { continue; }
       self.number_extension_token(item, …).await;
   }
   ```

   Templates still take their ids during the walk, so the pinned
   `<templatestyles/>{{Center|b}}` → style `#mwt2`/template `#mwt1` rule holds;
   the change is only *among* a chunk's extensions.

Half 1 alone changed nothing measurable (the extension was still numbered before
the style pass); half 2 alone was the earlier no-op (the extension was already
numbered by the nested pass). Together they give `Reflist` `#mwt123` style /
`#mwt124` references, as parsoid.

### Effect
`Zebro`: 62 583 → **63 888** (45.72% → 46.68%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409, `Unix` 4772), the fixture guard holds at 877/896, and
the workspace is green.

## Cite's `<references>` wrapper: the body, the columns class, the id, the rel
Four small differences in the reference list, each against PHP Cite's
`References::insertReferencesIntoDOM`:

- **`data-mw.body`** — a *tag pair* `<references>…</references>` records
  `"body":{"extsrc":""}`; a self-closing `<references/>` records no body.
  `Template:Reflist` builds its list with `{{#tag:references|…}}` (a tag pair),
  so the served output carries the empty body. `read_references` now reports
  whether the tag was self-closing (from the source's trailing `/>`) and
  `references_data_mw` emits the body accordingly.
- **`mw-references-columns`** — Cite adds it to the responsive wrapper when the
  group holds more than `CiteResponsiveReferencesThreshold` (10) notes.
- **The wrapper's node id** — Cite's `$refsNode` *is* the extension element, so
  it draws an id; rustoid built a fresh `<div>` without one, which shifted every
  id after it. `mark_for_id` (the empty-`data-parsoid` marker) is now applied to
  the wrapper.
- **`rel="mw:referencedBy"` on the multi-use back-link span** — Cite puts the
  relation on the `<a>` in the single-use form and on the wrapping `<span>` when
  a note has several uses.

### Effect
`Zebro`: 63 888 → **66 780** (46.68% → 48.79%). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction`
5161, `Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of
sovereign states` 8409, `Unix` 4772), the fixture guard holds at 877/896, and
the workspace is green.

## The next `Zebro` difference: a transclusion whose first child is a stylesheet
At 66 780, inside the `C. Nores et al.` note, `{{cite journal}}`'s transclusion is
placed differently. Parsoid puts it on the template's first rendered element, the
CS1 stylesheet:

```
<style data-mw-deduplicate="TemplateStyles:r1333433106"
       typeof="mw:Extension/templatestyles mw:Transclusion" about="#mwt8"
       data-mw='{"name":"templatestyles",…,"parts":[{"template":{"target":{"wt":"cite journal"…'
```

rustoid emits an empty wrapper and drops the sheet here:

```
<span class="mw-empty-elt" about="#mwt8" typeof="mw:Transclusion"
      data-mw='{"parts":[…cite journal…]}'></span><cite … about="#mwt8">
```

Two things are visible: the transclusion's `typeof`/`data-mw` did not merge onto
the stylesheet element (rustoid wrapped nothing in an `mw-empty-elt` span
instead), and the CS1 `<style>` is not emitted at this note at all (rustoid's
`TemplateStyles:r1333433106` first appears at a later note, with a different
`about`). This is the encapsulation merge again — `encapsulateTemplates` puts the
range's `about`/`typeof`/`data-mw` on the range's first element, and a stylesheet
is a rendering-transparent node — crossed with the templatestyles dedup ordering
inside a `<ref>` body. Left for the next session; noted rather than guessed at.

## A ref body's stylesheet, and where DedupeStyles runs
Chasing the "transclusion whose first child is a stylesheet" item turned up two
independent bugs, neither actually the encapsulation merge.

### A body expanded by `process_fragment_body` dropped its stylesheets
`process_fragment_body` built the body's DOM with a **private** fragment map and a
counter starting at 0. A `<templatestyles>` inside the body is resolved during the
expansion below it and stashed in `ext_fragments` (drained into the page's map only
at the end of `build_ast`), so the body's `mw:DOMFragment` placeholder named
nothing and the `<style>` vanished — the same shape as the media-caption bug, one
pipeline further in. Reduced: `A<ref>{{#invoke:list|unbulleted|a|b}}</ref>` left
the note with the `plainlist` `<div>` and no `Plainlist/styles.css`.

The body now allocates fragment ids from the document counter (`self.ext_next_id`)
and folds a **clone** of `ext_fragments` into its local map before building, so the
placeholder resolves while the page's own copies survive (a `take` would strand
them — the mistake recorded under the caption fix).

### Cite moves the notes in after DedupeStyles had already run
With the styles present, the byte totals jumped by ~180 KB on `Zebra`: rustoid
emitted a full `<style>` in every note where parsoid emits one plus
`mw-deduplicated-inline-style` links (21 vs 118 links). `dedupe_styles` ran
*before* Cite, so the stylesheets Cite moves out of the stashed ref bodies into the
reference list were never seen. Parsoid's `DedupeStyles` sees the final DOM, so the
pass now runs after the Cite block. `Zebra` returns to 534 KB against parsoid's
563 KB (was 511 KB with the styles simply missing).

### The single-use back-link is wrapped too
While reducing, `A<ref>text</ref>` showed parsoid wrapping **every** back-link in
`<span class="mw-cite-backlink">` — the `referencedBy` relation is on the `<a>` for
a single use and on the wrapping span for several. rustoid rendered the single-use
form bare (a comment claimed it had been "verified against a cached page"; the
transform endpoint and the served pages both wrap it). Fixed, with the two unit
tests that asserted the bare form updated.

### Effect
`Zebro`'s first difference is unchanged at 66 780 — the divergence there is a
different thing (see below) — but the reference lists now carry their stylesheets
and dedup links, and every page's byte total moves toward parsoid's (e.g. `Zebra`
510 675 → 534 265 against 562 730). Every first difference is unchanged, the
fixture guard holds at 877/896, and the workspace is green.

## What the 66 780 divergence actually is
It is *not* the merge of a transclusion onto its first element, and not the
literal `<templatestyles>` case: `A<ref>{{plain list|a}}</ref>` (a template body
beginning with a literal `<templatestyles>`) matches parsoid, which wraps the
sheet in `<span class="mw-empty-elt" about=… typeof="mw:Transclusion">` just as
rustoid does. Parsoid's `shouldStashRenderingTransparentNodes` refuses to wrap
when the node's *next* sibling carries the same `about`, which is why the plain
list is wrapped (the `<div>` follows) and the `#invoke:list`-only case is not.

The 66 780 case is `{{cite journal}}`, whose body is `<includeonly>{{#invoke:Citation/CS1|citation|…}}</includeonly>`
— the stylesheet is emitted by a nested *module* expansion. There the served
`<style>` carries both `mw:Extension/templatestyles` **and** `mw:Transclusion`,
with the extension's `name`/`attrs`/`body` *and* the transclusion's `parts` in
one `data-mw`, and rustoid instead emits the transclusion on an empty
`<span class="mw-empty-elt">` with the `<style>` beside it. So the wrapper must
merge onto a stylesheet that a *nested* expansion produced, not onto one written
in the body. Next to look at.

## 48.79% → 55.68%: the stylesheet renderer, the stylesheet merge, and Cite's note list
The 66 780 divergence — the previous section's "next to look at" — was not a
"nested expansion" difference at all. It was the rendering-transparent stash.
`DOMRangeBuilder::encapsulateTemplates` puts the transclusion metadata on the
range's first element (the CS1 `<style>`), and only *then*
`handleRenderingTransparentEltsBetweenBlocks` decides whether to move it into an
`mw-empty-elt` span. It refuses when the next element shares the target's `about`
— a `<style>` followed by the transclusion's own `<cite>` — which is exactly
`{{cite journal}}`. rustoid wrapped unconditionally, so the metadata landed on a
fresh span and the `<style>` stayed beside it with its own `about`.

The fix is in `tree_builder_html`: compute the stashable run and gate the wrap on
the same `should_stash` boundary test the trailing-run stash already used. When
the test fails *and* the target is a `mw:DOMFragment` placeholder (rustoid's
representation of a `<templatestyles>`), unfold it so the metadata lands on the
real `<style>` — merging the extension's `data-mw` (`name`/`attrs`/`body`) with
the transclusion's `parts` *appended*, the way `$encapDataMw->parts = $parts`
appends them. `Zebro`: 66 780 → 67 790.

### The stylesheet renderer was an approximation; it is now a port
Next came the CS1 stylesheet's *text*. rustoid rendered declaration values with a
hand-rolled walk (`normalise_value`), and it got strings, urls and whitespace
wrong: `quotes:'"' '"'` stayed single-quoted, `url(//…)` was left unquoted, and a
value split across a newline kept its tab. Parsoid does none of that — it
tokenizes with `Wikimedia\CSS\Parser\DataSourceTokenizer`, serializes each token
with `Token::__toString` (re-quoting strings and urls, escaping idents), marks
whitespace insignificant, and re-inserts a space only where `Token::separate`
says the neighbours would otherwise merge, `Util::stringify(minify)`.

So `pipeline/css.rs` is now that port: the tokeniser, `Token::__toString`,
`Token::separate`, and `stringify(minify)` (insignificant whitespace, `/**/`
between adjacent significant tokens, `calc()`-operator whitespace significant).
It replaces `normalise_value`, `normalise_at_prelude` and the selector scoper.
The scoper is now faithful too: `StyleRuleSanitizer` with
`hoistableComponentMatcher` hoists the longest leading run of `html`/`body`-led
compound selectors before the prepended `.mw-parser-output`, which is why
`html body.mediawiki .ambox` becomes `html body.mediawiki .mw-parser-output
.ambox` rather than rustoid's old `html .mw-parser-output body.mediawiki …`.

The differential test (`templatestyles_parsoid_test`) compares `render` against
the `<style>` body of every cached Parsoid rendering. It had been unusable: the
cache had been written under a different filename scheme than the test looked
for, so it skipped everything. It now reads the three schemes the cache has used
(`escape_body_stem`, `sanitize_path_separators`, the `:`-doubling legacy one),
keys each sheet by `(src, wrapper)`, and skips a sheet whose cached source
revision differs from the oracle's — otherwise the wiki's own later edit reads
as a render difference. That last one is real: `Module:Portal bar/styles.css`
was edited (a `var()` added) after every oracle was captured at r1371903450, and
comparing the newer source against the older rendering reports a difference that
is not rustoid's. **84 sheets checked, 83 byte-identical, 1 skipped as drift**
(was 77 of 84 matching).

### Three more divergences, each a byte of structure
- **Cite appends a newline after every note.** `RefGroup::renderReferenceListElement`
  ends with `$refsList->appendChild($ownerDoc->createTextNode("\n"))` (T372889),
  so the `<ol>` reads `<li>…</li>\n<li>…</li>\n</ol>`. rustoid built the list
  without the separator.
- **AddRedLinks must run after Cite.** A note's body only reaches the DOM when
  Cite renders the reference list, and Parsoid's `AddRedLinks` sees the final
  DOM: `{{cite journal}}`'s `[[Doi (identifier)|doi]]` carries `mw-redirect` on
  the served page. rustoid ran the pass before Cite, so the note links were never
  marked (34 `mw-redirect` on `Zebro` against rustoid's 10). Moving the
  collect-and-apply block after Cite fixes it; nothing between depends on it
  having run.
- **A literal HTML tag in link content is markup, not text.** CS1 renders an
  archive link as `[url <i>Title</i>]`; rustoid's `tokenize_link_content` only
  walked `{{…}}`/`-{…}-`/quotes, so the `<i>` became escaped text
  (`&lt;i>Title&lt;/i>`) in `[https://… <i>Title</i>]` and, inside a template,
  lost the `stx: "html"` that draws its node id. Adding a `try_html_tag` branch
  fixes the direct case. It is gated on the sanitizer's `AllowedLiteralTags`, not
  on `try_html_tag` alone: a disallowed tag (`<script>`) must stay text, and the
  sanitizer that would turn it back into text never visits tokens nested inside
  an attribute value, which is where link content sits when it runs. Ungated, it
  broke `media.txt`'s "Broken image links with HTML captions".

### Effect
`Zebro`: 66 780 → **76 212** (48.79% → **55.68%**). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction` 5161,
`Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of sovereign
states` 8409, `Unix` 4772), the fixture guard holds at **877/896**, the workspace,
clippy and fmt are clean.

## The next `Zebro` difference: a quote-generated `<i>` in link content has no DSR
At 76 212, inside the same CS1 note: parsoid's `<a rel="mw:ExtLink" …><i
id="mwAgs" data-parsoid='{"stx":"html"}'>Equids in Time and Space…</i></a>` has a
node id, rustoid's `<i>` does not, so every id after it is one behind.

The `<i>` is CS1's italicised book title (`utilities.wrap_style('italic-title',
…)`), i.e. wikitext `''…''` inside the link text. Minimal reproductions, both
against the transform endpoint:

```
A [https://example.com ''italic''] B   →  rustoid <i>italic</i>,  parsoid <i id="mwBA" data-parsoid='{"dsr":[23,33,2,2]}'>
A [[Foo|''italic'']] B                 →  rustoid <i>italic</i>,  parsoid <i id="mwBA" data-parsoid='{"dsr":[8,18,2,2]}'>
```

A page-level `A ''italic'' B` is correct (both give the `<i>` an id and a dsr),
so the gap is specific to link content. The cause is that rustoid re-tokenizes
link content with a *sub*-tokenizer (`tokenize_link_content`, and
`tokenize_caption_sol` for a wikilink caption) whose offsets are relative to the
content substring: the `mw-quote` tokens carry a `tsr` of `0..len(content)`, so
`QuoteTransformer` gives the `<i>` a `tsr` in the wrong coordinate space and
`ComputeDSR` (which ignores `tsr.source` and treats every offset as ambient)
cannot turn it into a page `dsr`. Parsoid tokenizes the link content in the
single grammar pass, so its offsets are absolute. The fix is to re-base the
content tokens' `tsr` by the content's start offset (and to the enclosing
source); it will want care because the same sub-tokenization feeds template
expansion, which currently relies on `tsr.source` to recover the argument text.

## Fixed: the sub-tokenizer's offsets now live in the page's coordinate space

Two independent faults produced the missing `<i>` id, and the second is the more
interesting one because the first hid it.

### Fault 1: the sub-tokenizer's `tsr` was link-relative

The hypothesis above was right about the mechanism and wrong to worry about
`tsr.source`. `PegTokenizer` already carries a `SourceRange`-per-token `source`,
but `make_dp` (used by `try_quote`, the template/tplarg parsers, the HTML-tag
parser and the extension parser) built a source-less `SourceRange::new`. The
sub-tokenizer that parses link content (`tokenize_directives_and_quotes`) ran with
`pos` starting at 0 over the content slice, so its `mw-quote` tokens carried
`tsr` `0..2` and `8..10` — the offsets *within the link text*.

The fix adds `PegTokenizer::base_offset`, set to the slice's start in the ambient
input by `with_base_offset`, and applies it in `tsr` and `make_dp` (and the one
hand-built `SourceRange::new` in the extension parser). The offset is threaded
from the three call sites that produce a slice of a larger input:

- `try_extlink` — the extlink text starts at `saved + 1 + content_src_start`;
- `try_wikilink` — the target and each `mw:maybeContent` part, at
  `saved + 2 + part_offset` (`split_wikilink_content_offsets` is the new
  offset-carrying variant of `split_wikilink_content`);
- `try_redirect` — the redirect target, at its own `target_start`.

A regression test (`extlink_content_quote_carries_page_absolute_tsr`) pins the
`mw-quote` `tsr` of `A [https://example.com ''italic''] B` to `(23,25)`/`(31,33)`.

### Fault 2: a blanket `clear_tsr` wiped the `mw-quote` anyway

With the offsets rebased, the tokenizer produced the right `mw-quote`, but
`on_ext_link` still saw a `mw-quote` with `tsr: None`. `Parser::expand_attr_tokens`

ran `token_utils::clear_tsr` over the whole expanded attribute value. That helper
recurses through attribute values (it was written for the `Israel` trap, where a
`{{PAGENAME}}` *inside a wikilink `href`* kept a `tsr` relative to the 12-character
sub-source and `DOMRangeBuilder` mistook the resulting marker for a top-level
transclusion at page offset 0). But it cleared *everything* in the value, not just
what an expansion produced — including the `mw-quote` tokens that were already
there. PHP's `Frame::expand` runs the chunk through the expansion pipeline, which
only *replaces* template/variable tokens and clears the `tsr` of what they produce
(`TemplateHandler::processTemplateTokens`); the untouched tokens keep theirs.

So the blanket `clear_tsr` is gone. It is safe to remove because Fault 1 was also
the root cause of the `Israel` trap: the `{{PAGENAME}}` in the wikilink `href` is
tokenized by `tokenize_link_target`, which now rebases, so the expansion marker's
`tsr` is page-absolute and can no longer masquerade as a top-level range. (`Israel`
itself still does not render — it hits the 60 s stall cap — so this is argued from
the mechanism, not measured on that page; every other page's first difference is
unchanged.)

### Effect
`Zebro`: 76 212 → **86 997** (55.68% → **63.56%**). Every other page's first
difference is unchanged (`Zebra` 2172, `Bicycle` 11836, `Help:Introduction` 5161,
`Nobel Prize` 5926, `Sundial` 3934, `Quicksilver (film)` 4213, `List of sovereign
states` 8409, `Unix` 4772). The fixture guard holds at **877/896**, the CSS
differential at 83/1-skip, and the workspace, clippy and fmt are clean.

### The next `Zebro` difference: a CS1 tracking category `<link>`
At 86 997, still in the notes list:

```
parsoid: …class="Z3988" about="#mwt115" id="mwAko"></span><link rel="mw:PageProp/Category" href="./Category:CS1_Spanish-language_sources_(es)" about="#mwt115" id="mwAks"/></span></li>
rustoid: …class="Z3988" about="#mwt115" id="mwAko"></span></span></li>
```

The CS1 module emits a tracking category (`Category:CS1 Spanish-language
sources (es)`); Parsoid renders it as an inline `<link rel="mw:PageProp/Category">`
inside the reference (the reference list is rendered in place, so the category
link stays inline rather than moving to the category section), and rustoid drops
it. The ids after it are one behind. That is the next difference to chase.
