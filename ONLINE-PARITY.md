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
| distinct Lua failures | 20 | 21 | **20** |
| entries in the Lua table | 34 | 31 | **29** |

Four pages that used to stall now render, and the leading Lua failure has changed
twice as entries were cleared:

- `Module:Lang` (`ustring.char`, 7 pages) — fixed.
- `mw.html` (attribute table, 4 pages) — fixed.
- `Module:Time ago` (2 pages) — fixed, and it needed *two* fixes: the format
  string and then the input parsing. It disappeared from the table only on the
  second run, which is the sort of thing that reads as a failed fix.

The top entry is now `Module:Unicode data` (7 pages), which is diagnosed above and
needs a design decision rather than a patch.

### The Lua failures are the work queue

They are concrete, each names a line, and the biggest is worth 7 pages:

- `Module:Unicode data:485` — `attempt to index a boolean value (field 'scripts')`.
  Diagnosed, and it is the interesting one. The module builds its data-module
  names at runtime:

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
  can only see the prefix `"Module:Unicode data/"` — the rest is computed. The
  truncated title is not the page, so the data is absent, `data = false`, and the
  caller then indexes it. The page `Module:Unicode data/scripts` does exist and is
  fetchable (checked against the wiki).

  **This needs a decision rather than a patch.** Making it work means letting
  `mw.loadData` reach the deferred-fetch loop *from inside a `pcall`*, which is a
  change to how a runtime module load is serviced — the error a `pcall` swallows
  is exactly the signal the loop keys on. Worth doing, and worth doing
  deliberately: the same dynamic-name pattern appears in the other data-heavy
  modules, so it is structural, not one page's quirk.
- `Module:Wikidata:247` — `invalid escape sequence near '"^\-'`. **This one is an
  interpreter-version gap, not a module bug, and it needs a decision.**

  The line is `mw.ustring.match(date, "^\-?%d+")`, and the service renders it
  with no script error. Real Lua *5.4* rejects that escape — the `lua` binary here
  is 5.5 and rejects it too — but Scribunto runs **Lua 5.1**, which treats an
  unknown escape as the bare character. rustoid embeds `lua54`:

  ```toml
  mlua = { version = "0.10", features = ["lua54", "vendored"] }
  ```

  `lua51` is available in the same crate. Switching would make rustoid match the
  interpreter Wikipedia's modules are actually written against, rather than being
  gratuitously stricter — but it changes behaviour for *every* module, so it is a
  deliberate call rather than a patch.

  Measured before deciding: with `lua51` the whole workspace passes except one
  test, `ustring_sub_clamps_instead_of_panicking`, which asserts 5.4's `string.sub`
  bounds behaviour. Scribunto's `mw.ustring` is a PHP-backed implementation and
  not plain 5.1 `string.sub`, so that test's expectation is the thing to verify
  against the service first — not the reason to stay on 5.4. The change was
  reverted untouched rather than forced through.
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
