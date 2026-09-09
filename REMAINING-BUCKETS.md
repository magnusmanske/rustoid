# Remaining fixture buckets (for future sessions)

Current baseline: **821/891 fixtures pass** (92%). Lib tests: 621 pass. Clippy: clean.
Working tree is clean. Commits are local (`main` is ahead of `origin/main`); **do not push** (user pushes).

Reference PHP Parsoid is pinned at `/tmp/parsoid-src` (HEAD `d79c17f03af7423c7c2dcc73d25a6f63a4b805e2`).
The PHP grammar (`src/Wt2Html/Grammar.pegphp`) and `TokenizerUtils.php` (esp. `inlineBreaks`)
are the authority; port their logic bit-for-bit.

## TableFixups status (this session)

A first cut of `TableFixups` is now wired in `rustoid-core/src/pipeline/table_fixups.rs` (runs after
`handle_link_neighbours`, before `cleanup`). Ported so far:
- Tokenizer-side table-cell temp flags (`TABLE_CELL_WITH_NO_ATTRIBUTE_SYNTAX`, `NON_MERGEABLE_TABLE_CELL`,
  `AT_SRC_START` — the last slot added, not yet set) on `TempData`, set in `tokenizer_v2.rs`
  (`try_table_data_tags`/`try_table_heading_tags`/`parse_tds`/`parse_ths`).
- `collect_attributish_content`, `attributish_prefix`, `reparse_templated_attributes` (the `k=v|` reparse),
  `hoist_transclusion_info` (partial), `drop_consumed_prefix`, `get_reparse_type` → `pipe_status_in_content`
  (now faithful: carries `in_tpl_content`/`about` across siblings, honors `shouldAbortAttr`), and a
  `split_hidden_cells` driver that moves subsequent children into each split cell and recurses so each
  split cell's own `k=v|` prefix reparses.
- The WRAPPER temp flag is now also stamped on `dp` (not just the `data-parsoid` string) for text-wrap
  encapsulation spans in `tree_builder_html.rs`, so `hoist_transclusion_info` can unwrap them.

**Fixed this session: the fostered-table encapsulation target** (`table_body_content_target` in
`tree_builder_html.rs`). The PHP `DOMRangeBuilder` (`findEncapTarget` + `MAP_TBODY_TR` migration) puts the
transclusion `about`/`typeof`/`data-mw` on the first *cell* (`<td>`/`<th>`) when the range extends past
the template (mixed template + wikitext, e.g. `{{table_attribs_4}} ||a||b` → `typeof` + `about` on the
first `<td>`, `about` on the sibling cells), and on the table *body* (`<tbody>`/`<thead>`/`<tfoot>`) when
the transclusion is well-balanced (single template produced the whole body, e.g.
`{{1x|{{!}} hi}}` → `typeof`/`about` on `<tbody>`). Rust now mirrors both by keying
`table_body_content_target` on whether the range extends beyond the template's DSR (`range_end > tpl_end`
⇔ not well-balanced). Net: **821/891** (up from 820); fixed "Template generated table cell with
attributes" without regressing "Image with table with rows from templates in caption".

Authoritative PHP output (confirm via `nativeTemplateExpansion:true` + `$env->pageCache`) for
`{{table_attribs_4}} ||a||b`:
```html
<td style="background-color:#DC241f;" width="10px" about="#mwt1" typeof="mw:Transclusion" …></td>
<td about="#mwt1">a</td><td about="#mwt1">b</td>
```
`about` on all three `<td>`s and `typeof="mw:Transclusion"` on the first (see line 1373 of tables.txt).

**Still failing** — table-cell template cluster:
- "4. Template-generated table cell attributes and cell content inside a templated table"
  (`{{tbl-start}}…{{tbl-end}}` wraps the whole `<table>`; needs `typeof` on `<table>`, not an empty span)
- "Templated table cell with untemplated attributes" (all variants: "Cell combination tests",
  "Integrated mode only", T343874)
- "Multiple transclusions in discarded table attribute position should be handled properly"
- The merge-cell path (ported in commit `3f240df`, currently harmless/no-regression) still needs its
  DSR/source-recovery and data-mw bookkeeping refined so the merged cell's `typeof`/`about`/`data-mw`
  match PHP byte-for-byte.

**Done this session:**
- `table_body_content_target` now targets `<td>` (mixed content) vs `<tbody>` (well-balanced), matching
  PHP's `findEncapTarget`; dropped the earlier incorrect `<tr>`/`<tbody>`-only targets.
- commit — `substitute_args` now unwraps `{{!}}` → `|` in *resolved argument values* (a magic pipe in a
  template argument always escapes to a literal pipe), while leaving source-level `{{!}}` intact for
  token handling. Fixes the `{{1x|1={{!}}title="fail"{{!}}bar}}` → `|title="fail"|bar` content before
  the next layer of the table-cell cluster.
- commit `6fccfbe` — "Accept `!!` in templates" + "Spec syntactic differences (`!!` vs `||`)".
- commit `3f240df` — ported `reparseWithPreviousCell` + `convertAttribsToContent` + `mergeCells` +
  `transferSourceBetweenCells` + `stripTrailingPipe`, wired `MAYBE_COMBINE_WITH_PREV_CELL` into
  `getReparseType`, and set `AT_SRC_START` (SOL cells) + `th`-leading-`!` non-mergeable in the tokenizer.

**Next step / root cause for "Templated table cell with untemplated attributes":**
`|class="foo"{{1x|1={{!}}title="fail"{{!}}bar}}` — the cell has *literal* attribute `class="foo"`
(no trailing `|`, so the tokenizer emits it as cell content; `row_syntax_table_args` backtracks), then a
template that expands to `|title="fail"|bar` (its leading `|` is the attr-content separator). Expected output
`<td class="foo">title="fail"|bar</td>` with `data-mw.parts=["|class=\"foo\"",{template}]`. Current actual:
`<td typeof="mw:Transclusion">class="foo"bar</td>` (the `{{!}}title="fail"{{!}}` collapses to `bar`, and
`class="foo"` isn't reparsed as an attribute).

The blocker is **token-level (not string-level) template-argument expansion** — the current async path
(`Parser::expand_one_template` → `substitute_args` → re-tokenize the whole string) cannot preserve the
distinction between `{{!}}` → `|` as *literal inline text* vs. *table-cell syntax*.

### Verified this session (against `/tmp/parsoid-src`, PHP 8.5)
PHP's `template_param_text` (Grammar.pegphp) tokenizes argument *values* with `nested_block<table=false,
extlink=false, templateArg=true, tableCellArg=false>` and `flattenIfArray`'s single-string back to a string.
Empirically (PegTokenizer dump):
- `{{1x|*bar}}` arg value → string `"*bar"` (lists are NOT formed at arg-tokenize time; they form when the
  spliced source is re-tokenized at SOL).
- `{{1x|1={{!}}title="fail"{{!}}bar}}` arg value → `[template(!), 'title="fail"', template(!), 'bar']` (the
  `{{!}}` stays a `template` token, NOT table syntax, because of `table=false`).
- Rust's `tokenize_directives` already matches this: `{{!}}`/`{{{x}}}` → `template`/`templatearg` tokens,
  bare text stays a string. **This is the correct primitive for argument values.**

The faithful token-level flow (PHP `expandTemplate` → `expandTemplateNatively`):
1. `AttributeTransformManager::process($frame, ['expandTemplates'=>false,'inTemplate'=>true], $attribs)`
   expands `{{{…}}}` in argument keys/values via `Frame::expand` *before* target re-resolution.
2. `processTemplateSource` → `wikitext-to-expanded-tokens` (`Tokenizer` + `TokenTransform2`) tokenizes the
   fetched source with `inTemplate=true`, `sol=true`; `Frame::expand` splices `templatearg` tokens.
3. `TokenStreamPatcher` (TT2 stage) re-tokenizes the spliced *string* runs with **SOL tracking**
   (`reprocessTokens($srcOffsets,$str,$sol)`), so `*bar` → listItem but `!!`/`||`/`{{!}}` cell continuation
   is preserved. This TT2 role of TokenStreamPatcher is **NOT yet ported** — the existing
   `pipeline/token_stream_patcher.rs` only covers the TT3 (tree-builder) role.
4. `{{!}}` magic word → `processSpecialMagicWord` → `'|'` (top level) or `<td attrSrc='' AT_SRC_START>`
   (`inTemplate`), which `TableFixups` then re-interprets.

### What was tried & reverted this session (no net win yet)
A coherent token-level attempt (`tokenize_directives` arg-values + `child_frame.expand` splice in
`expand_one_template` + `AttributeTransformManager` in `expand_templates` for parser-fn args) fixed the
`{{pre|123}}` family and `{{!}}` direction, but a naive `re_tokenize_string_runs` (re-tokenizing each string
run at fresh `sol=true`) regressed `!!`/`||` cell continuation (`3. Template-generated table cell…`) and
`{{!}}`-in-`format=wikitext` (`Template pre: Table`). Net 821→817. Root cause: string-run re-tokenization must
be **SOL-aware** (the TT2 `TokenStreamPatcher::reprocessTokens` port), not a fresh-SOL re-tokenize.

### Correct next increment (small, isolated commits)
1. ~~Port the TT2 `TokenStreamPatcher::reprocessTokens`/`onNewline` SOL-tracking string reprocessing~~
   **DONE** (`a7a904b`): added `sol`/`tplInfo['atStart']`/`inIndependentParse` state + the
   `T2529hack` string branch (a bare list/table-syntax string after a transclusion start meta
   re-tokenizes into a `listItem`/`table`), with unit tests. Net-neutral on the 821 baseline.
2. **Port `template_param_text` (inline tokenization of arg values).** `tokenize_directives` is too
   narrow — it only recognizes `{{…}}`/`{{{…}}}`/`-{…}-`/extensions and leaves wikilinks/quotes/entities
   as raw strings. PHP's `template_param_text` uses `nested_block<table=false, extlink=false,
   templateArg=true, tableCellArg=false>`, which in the (usual) mid-line position is `inlineline`:
   it tokenizes `[[Foo|bar]]` → `wikilink`, `'''b'''` → quote tags, entities, and nested templates,
   but NOT lists/tables (those form only at SOL after a newline). Verified empirically:
   `{{1x|[[Foo|bar]]}}` arg value → `wikilink` token; `{{1x|*bar}}` → string `"*bar"`; `{{1x|{{!}}…}}`
   → `[template(!), …]`. The Rust tokenizer already has `try_parse_inlineline`; add a
   `tokenize_template_arg_value` entry that reuses it with `templateArg=true, table=false`.
3. Wire `expand_one_template` to splice `templatearg` via `child_frame.expand` (NOT string
   `substitute_args`), and `{{!}}` → `|`/`<td>` via `process_special_magic_word` at the token level
   (`expand_templates` must emit `|`/`<td>` by `in_template`, setting `attr_src=''`+`at_src_start`;
   `handle_template` currently hardcodes `|`). Reeds `AttributeTransformManager::process` on template
   tokens for `{{{…}}}` in argument **keys/values** (fixes `{{#tag:pre|{{{1}}}|…}}`).

   ⚠️ A partial attempt (steps 2-3 without the faithful `template_param_text`, using
   `tokenize_directives`) regressed 821→800: arg values containing `[[…]]` stayed strings (breaking
   the linktrail/wikilink/pwrap/redirect clusters) because `tokenize_directives` doesn't inline-tokenize.
   The `{{!}}`/`3.`/`4.`/`Template pre: Table` cases stayed broken for the same reason. Do step 2 first.
4. Re-run `reparseTemplatedAttributes` (already ported) so the leading `|class="foo"` becomes the
   attribute and `title="fail"|bar` the content.

### Progress update (this turn: steps 2+3 attempted, reverted — 821→812, still -9)
Step 2 was implemented as `tokenize_template_arg_value` (full *inline* tokenization via
`try_inline_element`: `[[…]]`→wikilink, `'''…'''`→`mw-quote`, entities, `{{…}}`/`{{{…}}}`; newlines stay
text so multi-line values round-trip) and verified against PHP (`{{1x|[[Foo|bar]]}}`→wikilink,
`{{pre|'''123'''}}`→`[mw-quote,''',…]`, `{{1x|*bar}}`→string all match). Steps 2+3 together got **821→812**
(net -9): the `Template pre: Simple/Indent/Pwrap/List/Heading/Nowiki` cluster *fixed*, but six
`Template pre: Quotes/Link/Table` and two others *regressed*.

**Root cause of the remaining regression (the real blocker): token→source re-serialization at the
`{{#tag:…}}` parser-function boundary.** PHP's `ParserFunctions::tag_worker` (line ~434) **splices the arg
value tokens directly into the tag body** (`if (is_array($kv->v)) pushArray($toks,$kv->v)`), so a
`mw-quote`/`wikilink` token flows straight to TT3 (QuoteTransformer → `<b>`, etc.). Rust's
`pf_tag`/`tag_extension_token` instead *serialize* the content via `Item::Tok(t) => t.to_string()`
(`ParsoidToken::Display` → `<mw-quote/>`), irreversibly losing the quote/wikilink. So the remaining work is
**make `pf_tag`/`tag_extension_token` splice tokens (or re-serialize faithfully via `value`/`src`/`tsr`)
rather than `t.to_string()`** — exactly like PHP's `tag_worker` for the non-string arg case.

Also note: PHP's non-strict `TokenUtils::tokensToString` does **not** reconstruct `mw-quote`/`wikilink`
either (it only handles entities/listItem/urllink/DOMFragment), so the `#tag` path deliberately avoids
`tokensToString` and splices tokens; Rust's `tag_extension_token` must do the same.

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

### 2. Template-generated table cells + encapsulation (AttributeExpander + TableFixups)
The biggest remaining cluster. Needs `about`/`typeof="mw:Transclusion mw:ExpandedAttrs mw:LocalizedAttrs"`,
`data-mw` parts, `pi` arrays, `firstPipeSrc`, etc. **Root cause: a missing `TableFixups` DOM pass**
(`src/Wt2Html/DOM/Handlers/TableFixups.php`, ~1110 lines). rustoid already has `build_expanded_attrs`
+ `DataMwAttrib` (in `pipeline/attribute_expander.rs`) wired via `Parser::expand_attributes`; what's
missing is the *DOM post-processing* that re-interprets templated cell content as attributes.

#### Implementation plan (port `TableFixups::handleTableCellTemplates` + `reparseTemplatedAttributes`)
1. `collectAttributishContent(env, cell, templateWrapper)` — walk the cell's children accumulating
   text/`mw:Entity`/`mw:Transclusion` (hoisted `transclusions` list) / `mw:DOMFragment` (`<frag-marker>`),
   short-circuit on `shouldAbortAttr` (wikilink/figure). Returns `{txt, frags, transclusions}`.
2. `reparseTemplatedAttributes`:
   - regex `/(^[^|]+\|)([^|]|$)/D` on `txt` → `attributishPrefix` (splice `<frag-marker>` → fragment text).
   - **`tokenizeTableCellAttributes(prefix, false)`** — a NEW tokenizer entry using
     `row_syntax_table_args` (= `table_attributes<tableCellArg>`); returns `[attributes, spaces, pipe]`.
     (rustoid has `parse_row_syntax_table_args` already; add a public `tokenize_table_cell_attributes`).
   - `Sanitizer::applySanitizedArgs` → `sanitize_tag_attrs(name, attrs)` and `setAttribute` each kept attr.
   - set `MERGED_TABLE_CELL`, clear `TABLE_CELL_WITH_NO_ATTRIBUTE_SYNTAX`.
   - `hoistTransclusionInfo(dtState, transclusions, cell)` — lift the first transclusion's
     `about`/`typeof`/`data-mw` onto the `<td>` (and drop the inner `mw:Transclusion` span's about).
   - `setInnerHTML(cell, preg_replace('/^[^|]*\|/', '', innerHTML))` — drop consumed attr content.
3. `handleTableCellTemplates` driver — DOM traverse `td`/`th`, skip HTML cells/templated well-balanced
   tables, call `getReparseType` → `pipeStatusInContent` (for `MAYBE_REPARSE_ATTRS` when
   `TABLE_CELL_WITH_NO_ATTRIBUTE_SYNTAX`), recurse into any new split cells.
4. Wire a `table_fixups` pass into `Parser::build_ast` (after `tplwrap`, before `cleanup`/`redlinks`).

Tests: "1./2a./3./4. Template-generated table cell attributes…", "Template generated table cell with
attributes", "Templated table cell with untemplated attributes", "T343874", "Multi-line
extension/transclusion tags in a row", "Accept `!!` in templates", "Spec syntactic differences
(`!!` vs `||`)".
extension/transclusion tags in a row", "A table with …

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
