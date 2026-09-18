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
