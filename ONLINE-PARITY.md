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
