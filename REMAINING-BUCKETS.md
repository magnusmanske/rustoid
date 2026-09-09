# Remaining fixture buckets (for future sessions)

Current baseline: **815/891 fixtures pass** (91%). Lib tests: 614 pass. Clippy: clean.
Working tree is clean. Commits are local (`main` is ahead of `origin/main`); **do not push** (user pushes).

Reference PHP Parsoid is pinned at `/tmp/parsoid-src` (HEAD `d79c17f03af7423c7c2dcc73d25a6f63a4b805e2`).
The PHP grammar (`src/Wt2Html/Grammar.pegphp`) and `TokenizerUtils.php` (esp. `inlineBreaks`)
are the authority; port their logic bit-for-bit.

## Already resolved this session
- `{{!}}` as table-syntax pipe (commit `13265e8`).
- Table attribute values stop at cell separators in cell position (commit `bbbdd54`).
- Lone `|`/`!` are literal table-cell content; leading `||` empty-cell syntax (commit `9c1f01f`).
- Table row tags consume the full dash run (`{{!}}----`, `|----`) (commit `ddafda5`).
- Wikilink pipe trick `[[X|]]` is literal (commit `eeb2eb9`).
- `linkdesc` suppresses extlink/autolink in link-text context (commit `6248d6c`; T4095 v2, T2002, extlink precedence).
- ~~Valueless table-cell attrs discarded (not rendered as HTML)~~ (commit `28eb673`: sanitizer allowlist applied
  to wiki-syntax `table/tr/td/th/caption`; fixed "Table td-cell syntax variations", "! and || in td",
  "Invalid attributes in table cell T3830").
- Digest broken table-attr name chars `\0 / = >` as discarded KVs (commit `d05e221`).
- Context-dependent table attribute-name parsing (commit `ffc4b82`): start/row-tag attribute names
  are permissive (broken `||`/`|}`/`++` digested as valueless names); cell positions still terminate
  at `|`/`{{!}}`. Fixed "Digest broken attributes", "stray table end tags on start tag line".
- `|` stays literal in start/row-tag unquoted attribute values (commit `b012219`; "Pipe within attribute
  without quotes").

## Buckets (roughly ordered by priority / tractability)

### 1. Lookalike `||` / `!!` table edge cases (tokenizer, no template expansion)
- ~~"! and || in td attributes should not be parsed as `<th>`/`<td>`"~~ (done, `28eb673`)
- "Spec syntactic differences in parsing of `!!` compared to `||`" (template encapsulation)
- ~~"Simple table but with multiple dashes for row wikitext"~~ (done, `ddafda5`)
- ~~"Table td-cell syntax variations"~~ (done, `28eb673`)
- ~~"Pipe within attribute without quotes"~~ (done, `b012219`)
- ~~"A table with stray table end tags on start tag line (wt2html)"~~ (done, `ffc4b82`)
- ~~"Tables: Digest broken attributes on table and tr tag"~~ (done, `ffc4b82`)
- ~~"Invalid attributes in table cell (T3830)"~~ (done, `28eb673`)
- "Parsoid: Default to a newline after tables in new content (T53219)" (serializer)
- "Parsoid: Row-syntax table headings followed by comment & table cells"
- "Table security: embedded pipes", "Hacky use to indent tables, with comments (T65979)",
  "Wikitext tables can be nested inside HTML tables"

### 2. Template-generated table cells + encapsulation (AttributeExpander + TemplateHandler)
The biggest remaining cluster. Needs `about`/`typeof="mw:Transclusion mw:ExpandedAttrs mw:LocalizedAttrs"`,
`data-mw` parts, `pi` arrays, `firstPipeSrc`, etc.
- "1./2a./3./4. Template-generated table cell attributes and cell content …"
- "Template generated table cell with attributes"
- "Templated table cell with untemplated attributes" (+ "Cell combination tests", "Integrated mode only")
- "T343874: Templated table that has a templated cell with untemplated attributes"
- "Multiple transclusions in discarded table attribute position should be handled properly"
- "2b: Delete whitespace/comments if found in fosterable position while template-wrapping"
- "Accept `!!` in templates"
- "Templated table cell with untemplated attributes: Cell combination tests"

### 3. Wikilink edge cases
- ~~"Piped link with no link text"~~ (done, `eeb2eb9`: `[[X|]]` pipe trick is literal text)
- "T45661: Piped links with identical prefixes" — **red-link mock/harness default**: the PHP mock
  marks undefined titles `missing` (confirmed via `MockApiHelper::processQuery`), so Parsoid
  *correctly* produces a red link for `Prefixed article`; the legacy `!! html` golden shows blue.
  This is a harness data-seeding asymmetry, NOT a parser bug (needs a harness knownFailure/data
  decision, not a tokenizer change).
- "T4095: link with pipe and three closing brackets, version 2" and
  "T2002: [[page|http://url/]] should link to page, not http://url/" — **link-text extlink
  suppression**: inside a wikilink's link text (`link_text = link_text_parameterized<linkdesc=true>`),
  `[http://…]` must stay literal (not form an extlink). rustoid's `TokenizerOptions` lacks a
  `linkdesc` flag, so `render_wiki_link`'s caption re-tokenization (`tokenize_caption_sol`) wrongly
  forms an extlink. Fix: add `linkdesc` and gate `try_extlink` on it during link-text re-tokenization.
- "Link containing % as a single hex sequence interpreted to char"
- "Link containing double-single-quotes '' in text embedded in italics (T6598 check)"
- "T2002: [[page|http://url/]] should link to page, not http://url/"
- "Relative subpage noslash link"
- "Plain link in template argument", "Ensure that transclusion titles are not url-decoded"
- "Parsoid link trail/prefix/bracket escaping", "Parsoid T55221: entity-escaped wikilinks"
- "Parsoid-centric test: Whitespace in ext- and wiki-links should be preserved"
- "Wikilink extlink precedence", "Wikilinks with embedded newlines are not broken",
  "Broken wikilinks (but not external links) prevent templates from closing",
  "Nested wikilink syntax … parses as wikilink in extlink"
- "`<pre>` inside a link"

### 4. Selective-serialization (selser) / round-trip only
- "Invalid text in table attributes should be preserved by selective serializer"
- "T319143 - copy-pasting of cells …" (7 variants)

### 5. Misc / AttributeExpander / lists
- "Don't apply complex line-splitting heuristics in AttributeExpander for non-`<table>` tokens"
- "Multi-line extension tags should not interrupt table-cell parsing in the same row"
- "Asciiart template should display the right number of spaces"
- "Pipe in html attribute is link description"
- "Second colon on line: Templates involved and T2959 kicks in"
- "Parser function inside dl-dt list should be tokenized correctly",
  "Definition lists: ignore colons inside tags", "Mixed Lists: Test 11"
- "Hacky use to indent tables, with comments (T65979)", "Wikitext tables can be nested inside HTML tables"
- "Table cell attributes: Pipes protected by nowikis …", "Table security: embedded pipes"
- "Newline constraint after multi-node template"
- "Template interaction"
- T290526 family: "2. Using {{!}} in wikilinks", "Using {{!}} in template arguments, part 2",
  "T72875: brackets in attributes of elements in internal link texts",
  "T179544: {{anchorencode:}} output should be always usable in links"

## Key pitfalls (do not repeat)
- If `/tmp/parsoid-src/src/Wt2Html/Grammar.pegphp` looks truncated/empty, restore it:
  `cd /tmp/parsoid-src && git checkout -- src/Wt2Html/Grammar.pegphp` (the working tree
  sometimes loses it). Re-check `wc -l` ≈ 3249.
- `format!("{{{pipe}}}")` yields `"{|}"` (stray `}`); build `{|`/`{{{!}}` via `String::from("{")` + `push_str`.
- Attribute-name `{{` directive must be gated on `!table`, and `{`/`}` added to the table `is_stop` set, else `{{!}}` is absorbed as a cell attribute name.
- `parse_row_syntax_table_args` requires `pipe !pipe` and backtracks fully, so a bare word is not a valueless cell attribute.
- `at_cell_terminator`: lone `|`/`!` are literal content; only `||`, `|{{!}}`, `{{!}}|`, `{{!}}{{!}}`, `|}`, and (th) `!!` terminate. Newline terminates the inline run; the tree builder reassembles multi-line cells.
- `try_table_data_tags` must NOT reject leading `||` (empty-cell); `row_syntax_table_args` + `parse_tds` handle it.
- Table attribute values: `|`/`{{!}}` terminate only in *cell* position (`cell_arg=true`), not start/row-tag position (`table=false`).

## Validation
```bash
cd rustoid
cargo test -p rustoid-core --lib
cargo clippy -p rustoid-core --all-targets
cargo test -p rustoid-core --test integration_test test_all_parsoid_fixtures -- --nocapture 2>&1 | grep -a "Parsoid test results"
cargo test -p rustoid-core --test debug_failures -- --nocapture 2>&1 | grep -a "FAIL:"
```

### Empirical PHP reference
PHP 8.5 + vendor are present in `/tmp/parsoid-src`; `git sparse-checkout add baseconfig` restores the
`baseconfig/*.json` files the mock needs. `/tmp/pt_single.php` runs a single wikitext string through the
`MockSiteConfig`+`MockDataAccess`+wt2html pipeline:
```bash
php /tmp/pt_single.php "$'some wikitext'"
```
