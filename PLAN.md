# Rustoid — A Rust implementation of the Parsoid MediaWiki parser

## Overview

Rustoid is a from-scratch Rust reimplementation of the Wikimedia [Parsoid](https://www.mediawiki.org/wiki/Parsoid) parser. Parsoid is a bidirectional wikitext↔HTML5 parser used by MediaWiki. The existing canonical implementations are in JavaScript (Node.js) and PHP. This project aims to produce a parser with **identical output** to the PHP Parsoid, but with the performance, safety, and portability of Rust.

## Goals

1. **Byte-perfect output compatibility** with the PHP Parsoid (the current canonical implementation). Where byte-perfect is impossible (e.g., attribute ordering differences in HTML), whitespace-equivalent output is acceptable. The standard is: given the same wikitext, produce HTML that is functionally identical to Parsoid's and matches the `!! html/parsoid` sections in the official test suite.
2. **Full feature coverage**: wikitext→HTML, HTML→wikitext (round-tripping), selective serialization (selser), and the full VisualEditor editing pipeline (DOM diffing, minimal selser patches).
3. **Template expansion** (transclusion) via a pluggable data backend.
4. **Lua/Scribunto** module evaluation via the `mlua` crate.
5. **Format-agnostic internal representation** — the parser builds an intermediate AST/IR that can be lowered to HTML, JSON, Typst, PDF, etc.
6. **Pluggable data source** — a trait-based system that abstracts over a local indexed dump (`ruwex` crate) or the MediaWiki REST API.
7. **Parsoid test harness integration** — run the official Parsoid `parserTests.txt` files as Rust tests, targeting 100% pass rate on all test files.
8. **Production-grade library** with a demo binary for ad-hoc rendering.

## Non-goals (for now)

- In-process PHP extension support (use mw-api-callbacks instead).
- Full Tidy-compat output (Parsoid has shifted to RemexHtml-based output).

## Future stages (post v1.0)

These are documented here to ensure architecture decisions don't block them:

- **Full VE editing pipeline (Stage 2)**: The V2/V3 selser uses DOM diffing to produce minimal edit patches. Our Phase 8 implements basic selser; the full pipeline requires:
  - A DOM diff algorithm operating on our AST (port of Parsoid's `DOMDiff.php`).
  - Minimal selser that only modifies changed DOM regions while preserving surrounding wikitext including whitespace, comments, and separator newlines.
  - The ability to accept Parsoid-format HTML from VisualEditor, diff it against the original, and produce wikitext edits with surgical precision.
  - This is scoped as a v1.1/v2.0 deliverable. The AST and pipeline architecture is designed to support it from day one — the selser module in Phase 8 should already operate on DOM-diff inputs conceptually.
- **Citoid / citation auto-fill** integration.
- **Linter rule implementations** (the full set of Parsoid lint rules).
- **PageBundle REST API** endpoint (matching Parsoid's `/v3/transform/...`).
- **Multi-wiki pooling** (shared Lua runtime across wikis for efficiency).
- **WASM target** for in-browser rendering.

---

## Architecture

### Crate structure

```
rustoid/                    # workspace root
├── Cargo.toml              # workspace manifest
├── PLAN.md                 # this file
├── rustoid-core/           # core types, traits, and the parser engine
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── error.rs        # unified error types
│       ├── options.rs      # Parsoid configuration options
│       ├── traits.rs       # DataSource, SiteConfig, ExtensionHandler
│       ├── title.rs        # Title/namespace handling
│       ├── wikitext/        # wikitext tokenizer / preprocessor
│       │   ├── mod.rs
│       │   ├── tokenizer.rs   # low-level wikitext tokenizer
│       │   ├── preprocessor.rs # {{template}}, {{{args}}}, parser functions
│       │   └── tokens.rs      # token types
│       ├── pipeline/       # phased parsing pipeline
│       │   ├── mod.rs
│       │   ├── stage1.rs   # wikitext → tokens (with preprocessing/expansion)
│       │   ├── stage2.rs   # tokens → AST (HTML tree construction)
│       │   └── stage3.rs   # AST → final output / DOM cleanup
│       ├── dom/            # DOM-like AST types (NOT HTML-specific)
│       │   ├── mod.rs
│       │   ├── node.rs     # Node, Element, Text, Comment, etc.
│       │   ├── builder.rs  # tree builder
│       │   └── visitor.rs  # tree traversal utilities
│       ├── expand/         # template expansion
│       │   ├── mod.rs
│       │   ├── transclusion.rs
│       │   └── tpl_args.rs
│       ├── ext/            # extension tag handling
│       │   ├── mod.rs
│       │   └── registry.rs
│       ├── lua/            # Scribunto/Lua support
│       │   ├── mod.rs
│       │   └── engine.rs
│       ├── html/           # HTML output backend
│       │   ├── mod.rs
│       │   ├── serialize.rs  # AST → HTML string
│       │   ├── parse.rs      # HTML → AST (for round-tripping)
│       │   └── selser.rs     # selective serialization
│       ├── json/           # JSON output backend
│       │   └── mod.rs
│       ├── magic.rs        # magic words
│       ├── links.rs        # wikilinks, interwiki, external links
│       ├── sanitizer.rs    # HTML sanitization / attribute cleanup
│       ├── mw_api.rs       # MediaWiki API client (optional, behind feature)
│       └── ruwex_source.rs # ruwex dump backend (optional, behind feature)
├── rustoid-cli/            # binary demo
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
└── tests/                  # integration tests + Parsoid test harness
    ├── fixtures/           # symlinked or vendored parser test files
    └── test_harness.rs     # runner for parserTests.txt format
```

### Key design decisions

#### 1. Internal AST, not DOM

The parser builds an intermediate **AST** (`rustoid_core::dom::Node`) that is _not_ HTML-specific. Elements have a `kind` enum (`Paragraph`, `Heading(u8)`, `Table`, `Wikilink`, `Template`, `MagicWord`, …), plus an attribute map. HTML serialization is one backend; JSON, Typst, PDF backends can be added by implementing the `AstVisitor` trait.

This is important because it keeps the core clean and enables multi-format output. However, for Parsoid test compatibility, we MUST produce byte-identical HTML output to the PHP Parsoid, so the HTML backend will be the most developed.

#### 2. Trait-based data source

All data access goes through `DataSource` and `SiteConfig` traits:

```rust
/// Data source for wiki content
#[async_trait]
pub trait DataSource: Send + Sync {
    /// Fetch the raw wikitext for a page (by title).
    async fn get_page_content(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch a template expansion (the fully-expanded wikitext of a template page).
    async fn get_template(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch a Lua module source.
    async fn get_module(&self, title: &Title) -> Result<Option<String>>;

    /// Fetch file metadata (for [[File:...]] handling).
    async fn get_file_info(&self, title: &Title) -> Result<Option<FileInfo>>;

    /// Resolve a redirect.
    async fn resolve_redirect(&self, title: &Title) -> Result<Option<Title>>;
}

/// Site configuration needed for parsing
pub trait SiteConfig: Send + Sync {
    fn namespaces(&self) -> &HashMap<i32, NamespaceInfo>;
    fn interwiki_map(&self) -> &HashMap<String, InterwikiInfo>;
    fn magic_words(&self) -> &MagicWordMap;
    fn extension_tags(&self) -> &[String];
    fn base_url(&self) -> &str;
    fn article_path(&self) -> &str;
    fn server_url(&self) -> &str;
    fn language_code(&self) -> &str;
}
```

#### 3. Pipeline architecture

Parsing proceeds in three stages:

1. **Stage 1 — Preprocessing**: Tokenize wikitext, resolve templates/parser functions/magic words. This stage repeatedly expands until no more expansions remain (with recursion depth limits). Output: a flat token stream with all transclusions resolved.

2. **Stage 2 — Tree building**: Convert the token stream into an AST by building block/inline structure (paragraphs, headings, lists, tables, etc.). This mirrors Parsoid's `TreeBuilder` and the HTML5 tree construction algorithm adapted for wikitext.

3. **Stage 3 — Post-processing / serialization**: DOM cleanup (fixup fostered content, annotation ranges, section wrapping), then serialize via the chosen backend (HTML, JSON, etc.).

#### 4. Template expansion

Templates are expanded during Stage 1. When the tokenizer encounters `{{TemplateName|arg1|arg2=val}}`:

1. Look up `Template:TemplateName` via `DataSource::get_template`.
2. Parse the template wikitext.
3. Substitute arguments (`{{{1}}}`, `{{{arg2}}}`) — this is recursive; nested templates trigger further expansions.
4. Guard against infinite recursion (depth limit, typically ~40).

Parser functions (`{{#if:...}}`, `{{#switch:...}}`, etc.) are implemented as built-in handlers. Lua modules are invoked via `mlua`.

#### 5. Lua / Scribunto

The `mlua` crate provides a Lua interpreter. We sandbox it:
- Limited stdlib (no `os.execute`, no `io`, etc.)
- Timeout per invocation
- Memory limit

The `mw` global table is populated with MediaWiki API stubs (frame:expandTemplate, frame:preprocess, etc.). These _may_ call back into the `DataSource` trait.

#### 6. Parsoid options support

All Parsoid options must be supported. Key options:

| Option | Description |
|---|---|
| `body_only` | Omit `<html>`, `<head>`, `<body>` wrapper |
| `wrapSections` | Wrap `<section>` tags around headings+siblings |
| `section` / `section-ids` | Section editing anchors |
| `outputContentVersion` | e.g. `2.4.0`, `999.0.0` |
| `offsetType` | `byte`, `ucs2`, `char` |
| `pageBundle` | Bundle data-mw/data-parsoid in a JSON envelope |
| `lint` / `linting` | Enable lint error reporting |
| `parsoid` option string | `wt2html`, `html2wt`, `selser`, etc. |
| `annotations` | Whether to process annotation tags |

---

## Phase plan

### Phase 0 — Project setup (est. 1 day)

- [ ] Set up workspace with `rustoid-core`, `rustoid-cli` crates.
- [ ] Add core dependencies: `serde`, `serde_json`, `thiserror`, `tokio`, `reqwest` (for HTTP backends), `mlua`, `lazy_static`/`once_cell`, `regex`, `html5ever` (for HTML parsing during round-trip), `ruwex` (optional), `async-trait`.
- [ ] Set up CI (GitHub Actions) with `cargo test`, `cargo clippy`, `cargo fmt --check`.
- [ ] Create test fixture directory with script to download Parsoid parser test files.

### Phase 1 — Core types and traits (est. 2-3 days)

- [ ] Define `error.rs`: unified `Result<T, RustoidError>`.
- [ ] Define `options.rs`: all Parsoid configuration options.
- [ ] Define `traits.rs`: `DataSource`, `SiteConfig`, `ExtensionHandler`.
- [ ] Define `title.rs`: `Title` type with namespace-aware parsing/comparison.
- [ ] Implement mock `DataSource` for testing (in-memory hashmap of titles→wikitext).
- [ ] Implement mock `SiteConfig` with enwiki-like defaults.

### Phase 2 — Wikitext tokenizer (est. 3-5 days)

This is the foundational module. The tokenizer scans raw wikitext and emits a token stream. It must handle:
- [ ] Plain text
- [ ] Bold/italic wikisyntax (`'''bold'''`, `''italic''`, `'''''both'''''`)
- [ ] Wikilinks (`[[Page|display]]`, `[[:Category:Foo]]`)
- [ ] External links (`[https://example.com text]`, bare URLs)
- [ ] Template transclusions (`{{Name}}`, `{{Name|arg1}}`)
- [ ] Template arguments (`{{{1}}}`, `{{{name|default}}}`)
- [ ] Parser functions (`{{#if:...}}`, `{{#switch:...}}`)
- [ ] Magic words (`__TOC__`, `__NOTOC__`, `{{PAGENAME}}`, etc.)
- [ ] HTML comments (`<!-- ... -->`)
- [ ] HTML tags (both valid and wikitext-style, e.g. `<ref>`, `<pre>`)
- [ ] Nowiki (`<nowiki>`, `<nowiki/>`)
- [ ] Headings (`== Heading ==`)
- [ ] Horizontal rules (`----`)
- [ ] Lists (`*`, `#`, `;`, `:`)
- [ ] Tables (`{| ... |}`)
- [ ] Definition lists
- [ ] Preformatted text (leading spaces)
- [ ] Redirects (`#REDIRECT [[Target]]`)
- [ ] Behavior switches / indicators
- [ ] Extension tags (`<gallery>`, `<poem>`, etc.)
- [ ] Annotation tags (when `annotations=1`)
- [ ] Character entities (`&amp;`, `&#123;`)

**Token type** (key structure):

```rust
pub enum WikitextToken {
    Text(String),
    BoldOpen, BoldClose,
    ItalicOpen, ItalicClose,
    WikilinkOpen, WikilinkClose,
    ExtLinkOpen(String), ExtLinkClose,
    TemplateOpen(String /* name */),
    TemplateClose,
    TplArgOpen(String), TplArgClose,
    ParserFnOpen(String), ParserFnClose,
    MagicWord(String),
    Comment(String),
    HtmlTagOpen(String, Vec<Attribute>), HtmlTagClose(String),
    SelfClosingTag(String, Vec<Attribute>),
    NowikiContent(String),
    HeadingOpen(u8), HeadingClose,
    Hr,
    ListItem(char, u8 /* depth */),
    TableOpen, TableClose, TableRow, TableCell,
    Newline, ParagraphBreak,
    Redirect(String),
    ExtensionTag(String, Vec<Attribute>, String /* body */),
    AnnotationOpen(String, Vec<Attribute>), AnnotationClose(String),
    EOF,
    // ...
}
```

### Phase 3 — Preprocessor / Template expander (est. 5-8 days)

- [ ] Implement recursive template expansion.
- [ ] Implement parser function handlers:
  - `#if`, `#ifeq`, `#iferror`, `#ifexpr`, `#ifexist`
  - `#switch`
  - `#expr` (expression evaluator — can delegate to a simple shunting-yard parser or `meval`)
  - `#time` (date formatting — requires timezone database)
  - `#titleparts`
  - `#rel2abs`
  - `#tag`
  - `#invoke` (Lua delegation)
  - `#lst`, `#lsth`, `#lstx` (Labeled Section Transclusion)
  - `#property`, `#ask` (Semantic MediaWiki — optional)
- [ ] Implement variable substitution (`{{PAGENAME}}`, `{{FULLPAGENAME}}`, `{{CURRENTYEAR}}`, etc.)
- [ ] Implement recursive argument substitution with defaults.
- [ ] Guard against infinite recursion (depth counter).
- [ ] Handle `{{!}}` magic (table pipe escape in templates).
- [ ] Handle `<includeonly>`, `<noinclude>`, `<onlyinclude>` in template source pages.

### Phase 4 — Lua / Scribunto engine (est. 3-5 days)

- [ ] Integrate `mlua` and create sandbox.
- [ ] Implement `mw` global table:
  - `mw.site` (site info)
  - `mw.title` (title object)
  - `mw.uri` (URL utilities)
  - `mw.text` (text utilities: encode, decode, trim, etc.)
  - `mw.language` (language/formatting)
  - `mw.message` (i18n message lookup, via DataSource)
  - `mw.html` (programmatic HTML builder — common in Lua modules)
  - `mw.ustring` (Unicode-aware string operations)
- [ ] Implement `frame:expandTemplate()` → calls back into template expander.
- [ ] Implement `frame:preprocess()` → calls back into preprocessor.
- [ ] Implement `frame:callParserFunction()`.
- [ ] Implement `frame:extensionTag()`.
- [ ] Implement `frame:newChild()` / parent frame management.
- [ ] Implement `mw.log` (logs to a callback).
- [ ] Implement timeout (Lua hook that errors after N instructions).
- [ ] Implement memory limit check.

### Phase 5 — AST / Tree builder (est. 4-6 days)

- [ ] Define AST node types (`NodeKind` enum: `Element`, `Text`, `Comment`, `ProcessingInstruction`, `Document`).
- [ ] Define element kinds (`ElementKind` enum: `Paragraph`, `Heading(u8)`, `List(ListType)`, `ListItem`, `Table`, `TableRow`, `TableCell`, `Bold`, `Italic`, `Link`, `Image`, `ExtensionTag`, `Transclusion`, `Annotation`, …).
- [ ] Implement tree builder that converts flat token stream to nested AST.
- [ ] Implement paragraph wrapping logic (matching Parsoid's `pWrap`).
- [ ] Implement list/table builders.
- [ ] Implement heading auto-numbering and TOC generation.
- [ ] Implement foster parenting (content that moves out of tables into foster boxes).
- [ ] Implement section wrapping.
- [ ] Implement DSR (DOM Source Range) computation on all nodes (byte offsets into original wikitext).

### Phase 6 — HTML serialization (est. 3-5 days)

- [ ] Implement `ast_to_html` visitor:
  - Emit HTML5 with proper DOCTYPE, charset, etc.
  - Emit `data-parsoid` attributes (DSR, tsr, tmp info).
  - Emit `data-mw` attributes (template parts, extension data, annotations).
  - Emit `typeof` attributes (`mw:Transclusion`, `mw:Extension/...`, `mw:Placeholder`, etc.).
  - Emit `about` attributes for transclusion grouping.
  - Handle language/variant attributes (`lang`, `dir`).
  - Handle sanitized attributes (href, src, style cleansing).
- [ ] Media handling: generate appropriate `<figure>`/`<span>` structures for images, audio, video.
- [ ] Gallery handling.
- [ ] Reference/Footnote (`<ref>`) rendering.
- [ ] Table of Contents generation.
- [ ] Indicator rendering.
- [ ] Category link rendering.

### Phase 7 — HTML→wikitext (round-tripping) (est. 4-7 days)

- [ ] Implement `html_to_ast` using `html5ever`:
  - Parse Parsoid-format HTML back into AST.
  - Extract `data-parsoid` and `data-mw` JSON.
  - Reconstruct wikitext token structure.
- [ ] Implement `ast_to_wikitext` serializer:
  - Emit wikitext from AST nodes.
  - Use DSR information for optimal serialization.
  - Handle template serialization (reconstruct `{{...}}` from `data-mw.parts`).
  - Handle extension tag serialization.
  - Handle annotation serialization.
  - Handle whitespace/separator rules (the Parsoid separator algorithm).

### Phase 8 — Selective serialization (selser) (est. 5-8 days)

- [ ] Implement the basic selser algorithm:
  - Given original wikitext, original HTML, and modified HTML, produce modified wikitext.
  - Use DSR (DOM Source Range) information to preserve unmodified portions.
  - Detect and serialize only changed DOM regions.
  - Handle edge cases: modifications that change paragraph boundaries, foster content, table structure.
- [ ] Implement DOM diffing (port Parsoid's `DOMDiff.php` logic):
  - Compare original and modified HTML DOM trees.
  - Identify inserted, deleted, moved, and modified nodes.
  - Track DOM changes with positional markers for selser.
- [ ] Implement minimal selser:
  - Only emit wikitext for changed DOM subtrees.
  - Preserve original wikitext whitespace, comments, and separators around unmodified regions.
  - Handle template/extension boundaries correctly.
- [ ] Test with `selserWrappingParserTests.txt`, annotation selser tests, and the full suite of selser-specific tests.
- [ ] Test with VE-like edit scenarios (insert paragraph, delete heading, modify table cell).

This phase lays the groundwork for the full VE editing pipeline (planned for v1.1/v2.0, see "Future stages" above). The DOM diffing and minimal selser components are designed to be extended later with advanced heuristics for optimal edit patches.

### Phase 9 — Data source implementations (est. 2-3 days)

- [ ] `MockDataSource` (already done for testing, but make it public).
- [ ] `MediaWikiApiDataSource`:
  - Uses `reqwest` to call `action=query`, `action=parse`, `action=expandtemplates`.
  - Caches responses in memory (with TTL).
  - Handles API error responses gracefully.
- [ ] `RuwexDataSource` (behind `ruwex` feature flag):
  - Reads indexed dump files produced by `ruwex`.
  - Maps page titles to offsets in the dump.
  - Fast random access to pages in a multi-GB dump.

### Phase 10 — Parsoid test harness (est. 3-5 days)

- [ ] Write a parser for the `parserTests.txt` format:
  - `!! Version`, `!! article`, `!! test`, `!! options`, `!! wikitext`, `!! html/parsoid`, `!! html/parsoid+lang`, `!! html/parsoid+integrated`, `!! end`, `!! article`.
  - Support for the JSON `parsoid=` option (specifying modes).
  - Support for `!! html/parsoid here` (alternative expected HTML).
  - Support for `!! html/parsoid+lang` (language variant output).
  - Support for `!! wikitext/edited` (selser tests).
  - Support for `!! html` (legacy parser output, used for comparison; we may skip these).
- [ ] Write a test runner (`tests/test_harness.rs`):
  - For each test: parse wikitext, compare output HTML to `!! html/parsoid` section.
  - Support `wt2html`, `html2wt`, `selser`, `wt2wt` modes.
  - Load and respect `*-knownFailures.json` files to skip known failures.
  - Report pass/fail statistics.
- [ ] Vendor test files:
  - Write a script (`scripts/fetch_tests.sh`) that downloads all `parserTests.txt` files from:
    - `parsoid/tests/parser/` (Parsoid-specific tests: annotations, attribute expansion, dom normalizer, encap, i18n, fragment handling, section wrapping, selser wrapping, separators, table fixups, tree builder, v3 parser functions)
    - `parsoid/tests/parser/` (synced copies of MediaWiki core tests: comments, badCharacters, definitionLists, extLinks, headings, indentPre, indicators, interlanguageLinks, interwikiLinks, langParserTests, magicLinks, magicWords, media, preTags, preprocessor, pst, pWrapping, quotes, redirects, tables, wtEscaping)
    - `parsoid/tests/parser/` (other: poemParserTests, timedMediaHandler, regressions)
  - Store in `tests/fixtures/`.
- [ ] CI integration: run test harness on every PR (may need `--include-only` for fast CI subset).

### Phase 11 — HTML→wikitext test pass (est. 3-4 days)

After the test harness is set up, we systematically fix failures:

- [ ] Work through `wt2html` test failures first (these cover basic parsing).
- [ ] Work through `html2wt` test failures (HTML→wikitext round-trip).
- [ ] Work through `wt2wt` test failures (wikitext→HTML→wikitext).
- [ ] Work through `selser` test failures.
- [ ] Update `*-knownFailures.json` as we fix things.

### Phase 12 — CLI binary (est. 1-2 days)

- [ ] `rustoid-cli` binary:
  - `rustoid render --page "Main Page"` → renders to HTML and prints to stdout.
  - `rustoid render --page "Main Page" --format json` → JSON output.
  - `rustoid roundtrip --page "Foo"` → parses then serializes back to wikitext, checks consistency.
  - `rustoid test --file tests/fixtures/parserTests.txt` → run test harness.
  - `rustoid serve` → starts a small HTTP server mimicking the Parsoid REST API.
  - Support `--source=api --api-url=https://en.wikipedia.org/w/api.php` and `--source=ruwex --dump=path/to/dump.xml.bz2`.

### Phase 13 — Polish / hardening (est. 3-5 days)

- [ ] Fix all clippy warnings.
- [ ] Documentation: module-level docs, examples in `rustoid-core` docs.
- [ ] Performance profiling and optimization (flamegraph, criterion benchmarks).
- [ ] Fuzz testing (cargo-fuzz on the tokenizer and preprocessor).
- [ ] Add `rustdoc` and publish to docs.rs.
- [ ] Write a CONTRIBUTING guide.
- [ ] Test with real Wikipedia pages (sample set of ~10k articles).
- [ ] Compare output against PHP Parsoid using the `compare` tool from Parsoid's test suite.

---

## Dependencies

| Dependency | Version | Purpose |
|---|---|---|
| `serde` / `serde_json` | 1 | Configuration, data-mw/data-parsoid serialization |
| `thiserror` | 2 | Error types |
| `tokio` | 1 | Async runtime |
| `reqwest` | 0.12 | HTTP client for MediaWiki API |
| `async-trait` | 0.1 | Async trait support |
| `mlua` | 0.10 | Lua engine for Scribunto |
| `html5ever` | 0.29 | HTML5 parsing for round-tripping |
| `regex` | 1 | Wikitext pattern matching |
| `once_cell` | 1 | Lazy statics |
| `url` | 2 | URL parsing |
| `chrono` | 0.4 | Date/time for `#time` parser function |
| `ruwex` | 0.1 | Indexed dump access (optional, behind feature) |
| `clap` | 4 | CLI argument parsing |
| `tracing` / `tracing-subscriber` | 0.1 | Logging |

---

## Test file format

Parsoid's `parserTests.txt` format is a simple line-based format:

```
!! Version 2

!! article
Template:Foo
!! text
Template content here
!! endarticle

!! test
Description of the test
!! options
parsoid=wt2html,selser
annotations=1
language=fr
!! wikitext
'''bold text'''
!! html/parsoid
<p><b>bold text</b></p>
!! html/parsoid+lang
<p><b>texte en gras</b></p>
!! wikitext/edited
''bold text''
!! end
```

Key sections:
- `!! article` / `!! text` / `!! endarticle` — define a mock article (e.g., a template).
- `!! test` — begins a test case.
- `!! options` — parser options (JSON or key=value).
- `!! wikitext` — input wikitext.
- `!! html/parsoid` — expected HTML output (Parsoid format).
- `!! html/parsoid+lang` — expected output in language variant.
- `!! html/parsoid here` — alternative expected output that starts comparison from `here`.
- `!! html` — legacy parser expected output (usually ignored).
- `!! html/parsoid+integrated` — integrated mode output.
- `!! wikitext/edited` — for selser: the modified wikitext expected.
- `!! end` — ends the test.

The `*-knownFailures.json` files list test names that are expected to fail (regressions we haven't fixed yet).

### Byte-perfect comparison strategy

Achieving byte-perfect output requires a precise comparison strategy:

1. **Normalized comparison**: Before comparing, both expected and actual HTML are parsed and re-serialized through a canonical HTML5 serializer (spec-compliant, deterministic attribute ordering, consistent entity encoding). This catches functional differences while tolerating semantically equivalent variation.

2. **Strict comparison**: After normalized comparison passes, run a byte-level `assert_eq!` against the expected output string. This is the gold standard and should be the eventual target for all tests.

3. **Whitespace tolerance**: The test harness supports a `--tolerant` mode that normalizes whitespace before comparison. Tests that pass only in tolerant mode are tracked separately (whitespace bugs).

4. **Test categories**: The harness tags each failure with a reason:
   - `BYTE_MISMATCH` — output differs at byte level (but may be semantically equivalent).
   - `WHITESPACE_ONLY` — only whitespace differs.
   - `ATTRIBUTE_ORDER` — HTML attributes in different order (canonicalization should fix this).
   - `ENTITY_ENCODING` — different entity encoding choices (e.g., `&#39;` vs `'`).
   - `FUNCTIONAL` — actual parsing difference.

5. **Progress tracking**: The `*-knownFailures.json` files include a `reason` field for each failure so we can track which category of bugs remains.

---

## Milestones

| Phase | Deliverable | Success criteria |
|---|---|---|
| 0 | Project skeleton | `cargo build` succeeds |
| 1 | Core types | Mock DataSource passes basic tests |
| 2 | Tokenizer | Tokenizes simple wikitext correctly |
| 3 | Preprocessor | Expands `{{1x|hello}}` correctly |
| 4 | Lua engine | Executes `mw.site.siteName` |
| 5 | Tree builder | Converts tokens to AST |
| 6 | HTML backend | Renders basic wikitext to correct HTML |
| 7 | Round-trip | HTML→wikitext→HTML produces identical output |
| 8 | Selser | Modified HTML produces correct minimal wikitext diff (including DOM diffing) |
| 9 | Data sources | Can render a page from enwiki API |
| 10 | Test harness | Runs parserTests.txt, reports results |
| 11 | Test pass | >95% of parserTests.txt passing; byte-perfect on all passing tests |
| 12 | CLI | `rustoid render --page "Main Page"` works |
| 13 | Polish | All clippy clean, docs published; >99% test pass rate targeted |

---

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Wikitext is extremely complex; edge cases are numerous | Use Parsoid test suite from day 1 (Phase 10); let tests drive fixes. Aim for byte-perfect compatibility on every test. |
| Byte-perfect HTML output is hard due to attribute ordering, whitespace handling, entity encoding choices | Build a diff-based test comparator that normalizes only the things Parsoid itself considers equivalent (canonical HTML serialization). Track exact differences in known-failures. |
| Lua module compatibility | Start with a restricted subset; test against frequently-used Wikipedia Lua modules (Module:Citation, Module:Infobox, Module:Wikidata). The `mlua` engine is pure Rust and may have subtle behavioral differences from LuaJIT — test early. |
| Performance | Profile early; use `criterion` benchmarks; keep AST lazy where possible. Rust should give us a natural advantage over PHP. |
| PHP Parsoid changes over time | Pin to a specific Parsoid commit for test files; set up a periodic test-file refresh script. |
| Template expansion infinite loops | Hard recursion limit; timeout-based guard. |
| Selser is very complex (especially the full VE pipeline) | Implement basic selser in Phase 8 with DOM diffing; scope advanced VE optimizations to v1.1+. Architecture supports extending the diff/selser later. |
| Large test suite (thousands of tests) takes too long to iterate on | Parallel test execution; ability to run subsets (`--filter` flag in test harness). Fast CI subset for PRs, full suite nightly. |

---

## Coding conventions

- Follow Rust idioms: prefer `enum` over stringly-typed variants, use `Result<T, E>` pervasively.
- Keep functions small (<50 lines preferred).
- Use `tracing` for logging (not `println!`).
- All public items must have doc comments.
- Tests in `#[cfg(test)]` modules or `tests/` directory.
- Async where needed for I/O (data source calls), but the parser core should be sync.
- `#![warn(missing_docs)]` on all crates.

---

## References

- [Parsoid project overview](https://www.mediawiki.org/wiki/Parsoid)
- [Parsoid on Gerrit](https://gerrit.wikimedia.org/g/mediawiki/services/parsoid/)
- [Parsoid on GitHub](https://github.com/wikimedia/mediawiki-services-parsoid)
- [Parsoid/DeveloperSetup](https://www.mediawiki.org/wiki/Parsoid/DeveloperSetup)
- [Parser test format](https://www.mediawiki.org/wiki/Parsoid/ParserTests)
- [Scribunto Lua reference](https://www.mediawiki.org/wiki/Extension:Scribunto/Lua_reference_manual)
- [Parsoid HTML specification](https://www.mediawiki.org/wiki/Specs/HTML)
- [Ruwex crate](https://codeberg.org/magnusmanske/ruwex)
- [MediaWiki API:Parse](https://www.mediawiki.org/wiki/API:Parse)
