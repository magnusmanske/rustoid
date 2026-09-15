# Remaining fixture buckets (for future sessions)

Current baseline: **865/891 fixtures pass** (97%). Lib tests: 669 pass. Clippy: clean.

Note: the harness compares against `!! html/parsoid+standalone`, falling back to
`!! html/parsoid+integrated` when there is no standalone section (mirroring PHP
`Test::normalizeHTML`). A `+integrated`-only fixture is one PHP's own runner
**skips** in standalone mode, so its expectation is unreachable for rustoid. 36
fixtures are in that state; most still compare equal, so the fallback is left in
place (skipping them would hide real divergences rather than surface them).

## Landed: indent-pre nowikis and `hasBlocksOnLine` (859 → 860)

`Parsoid: Default to a newline after tables in new content (T53219)` now
passes. Two linked gaps in the html2wt serializer:

1. **The indent-pre escape only fired in multiline mode.**
   PHP's condition is `$indentPreUnsafe && ( !hasBlocksOnLine(
   $state->currLine->firstNode, true) || !empty($opts['inMultilineMode']) )`;
   the `hasBlocksOnLine` term was missing, so a space at start of line was
   returned verbatim and re-read as indent-pre. Ported `hasBlocksOnLine` and
   `startsOnANewLine`, and `escapedText` now protects *just the space* with
   `<nowiki> </nowiki>` (PHP splits on `/(^|\n) /`) instead of wrapping the
   whole text.

2. **`stripUnnecessaryIndentPreNowikis` was never ported.**
   `has_indent_pre_nowikis` was set but never read, so the protecting nowiki
   survived into the output. The pass now runs on the assembled output,
   dropping the nowiki *and* the whitespace it protected unless the content
   would read as a sol-sensitive construct (`=*#:;`), in which case a
   self-closing `<nowiki/>` remains.

Verified against Parsoid itself: `</table> bar` → `|}\nbar`, but
`</table> =bar` → `|}\n<nowiki/>=bar`. PHP's two site-config regexps behind
this pass have no rustoid equivalent, so the constructs are recognised
structurally (`sol_transparent_no_ws_len` / `is_sol_transparent_line` over
comments, category links and behaviour switches).

## Landed: `wrapperUnmodified` and the `+` combinator (no fixture change)

Two faithful fixes that `Invalid text in table attributes should be preserved
by selective serializer` also needs (it still fails on a third issue, below).

1. **`wrapperUnmodified` was not modelled.** PHP's `DOMHandler::handle` takes
   it, and `serializeTableTag` returns `getOrigSrc($dsr->openRange())` when
   set. Without it a table tag whose only change is in a descendant was
   re-serialized from the DOM, losing non-attribute wikitext inside the tag
   and normalizing quoting (`{| <span>boo</span> style='border:1px solid
   black'` came back as `{| style="border:1px solid black"`). The trait, all
   handlers, and the call site now thread it.

2. **The change-application selector engine only supported the descendant
   combinator.** The fixture's `td + td` change matched nothing and was
   *silently* dropped, so cell 2 kept its old text. PHP's
   `querySelectorAll` would have matched — and its runner errors out when a
   selector matches nothing, which is why the silence was easy to miss.
   `parse_selector` now records the combinator between compounds and
   `matches_steps` walks the previous-element-sibling chain for `+`.

## Landed: separator constraints for text nodes (860 → 861)

`Invalid text in table attributes should be preserved by selective serializer`
now passes. The previous session's note blamed `make_separator` or
`last_source_node`; both were innocent. The real gap was one step earlier.

**`serialize_node`'s text branch never called
`updateSeparatorConstraints`.** PHP does it for every node, but text nodes get
`new DOMHandler( false )` — a handler whose `before`/`firstChild` hooks return
*empty* constraints, so only the *previous* handler's constraints contribute.
Skipping the call left the text node with no `constraint_info` at all, so
`buildSep` never learned that the separator is `SepType::ParentChild`. The
trimmed-whitespace recovery (`separators.rs:263-272`) is guarded on exactly
that type, so the cell's recorded `leadingWS = 1` was never consulted and
`| 1` serialized as `|abc` instead of `| abc`.

The `currNodeUnmodified` flag is now also computed on this path (guarded on
`selser_mode`), matching the text branch of PHP's `serializeNodeInternal`.

Worth noting for the next session: the *DSR* side was already correct — both
cells parsed to `leading_ws: 1`, byte-identical to PHP. When a selser
recovery branch looks dead, check that the constraint plumbing that gates it
actually ran before suspecting the recovery logic itself.

## Landed: token-level argument substitution (858 → 859)

`Plain link in template argument` now passes, via three fixes.

The premise in the previous session's note was wrong in an instructive way:
the live expansion path (`Parser::expand_one_template`) **already** substituted
token-level. `TemplateHandler::expand_template_natively` — the string-level
`substitute_args` implementation the note was aimed at — turned out to be
**dead code referenced only by its own test**, long since superseded. The real
gaps were narrower:

1. **`ParamInfo` wikitext from the source span.**
   `TemplateEncapsulator::prepareTplParamInfos` takes each argument's `kWt`/
   `vWt` from the KV's `srcOffsets` (`$srcOffsets->value->substr($src)`), not
   from stringifying its tokens — because `TokenUtils::tokensToString` has no
   arm for a `wikilink`/`extlink`, so a link argument stringifies to `""` and
   `data-mw` records `"1":{"wt":null}`. rustoid set no spans on template-token
   arguments at all. The tokenizer now records them; `KVSourceRange` carries
   its own `source` (mirroring `SourceRange::fromSource`/`substr`), so a token
   tokenized from a template body still recovers its own wikitext.

   Note the positional/named decision turns on `key.end === value.start`, so it
   *requires* `srcOffsets`. With `None`, PHP treats a blank key as **named**;
   the old rustoid test asserting positional encoded rustoid behaviour, not
   PHP's, and was corrected.

2. **A `|` inside an external link separates template arguments.**
   `inlineBreaks` breaks on `|` when the `templateArg` stop is set and never
   consults `extlink` (which only guards `]`). The `extlink` guard is retained
   for wikilink content, where `link_text_parameterized` treats `[...]` as an
   extlink atom — the `Images with the "|" character in the comment` case.

3. **Two bounds on the template-close scan.**
   * A `}}` reached while a `[[` is still open fails the scan *immediately*
     rather than skipping ahead. Without this, `{{1x|[[http://x |y]}}` (a lone
     `]`, then `}}`) swallowed the *next line's* `]]` and expanded across both
     — PHP backtracks the whole `template` production at that point, so rows 3
     and 4 of the fixture must parse independently. Verified against PHP's own
     tokenizer: it yields `"{{1x|["` + `extlink(|123)` + `"}}"` then a
     separate `template` token.
   * `try_extlink` treats a leading `|` in the link content as a break only
     inside a table cell (`tableCellArg`); PHP's `|` stop is context-dependent.
     Outside a cell `[http://x |123]` is an extlink whose text is `|123`, which
     is what makes the invalid-title bail (`bailTokens`) re-tokenize
     `[[http://x |123]]` to PHP's `[` + extlink + `]` shape.

## Landed: a batch of five, 861 → 865

The previous "remaining 14" note was partly wrong, and the corrections are worth
recording because they cost time to find:

1. **`Wikitext tables can be nested inside HTML tables`** — *not* an
   `AttributeExpander` problem. `is_table_tag` omitted `tbody`/`thead`/`tfoot`,
   so `WTUtils::inHTMLTableTag`'s walk up to the `stx='html'` table stopped at
   the parser-inserted `<tbody>` and `serializeChildTableTagAsHTML` was false.

2. **`Second colon on line: Templates involved and T2959 kicks in`** — *not*
   `AttributeExpander` either. `ListHandler::onAny` had PHP's `if/elseif`
   polarity inverted, so `inT2529Mode` was cleared as soon as it was set, and
   `onListItem` never reset `haveDD`; the stale flag made a `:` serialize as
   cell content instead of opening a `<dd>`.

3. **`Parser function inside dl-dt list should be tokenized correctly`** — the
   tokenizer was correct; `#dir` simply was not implemented, and `resolve_target_string`
   could not have resolved it anyway. See below.

4. **`T45661: Piped links with identical prefixes`** — the note claiming the
   fixture "is not in the fixture's articles" was **wrong**: `prefixed article`
   is defined at `wikiLinks.txt:53`. `case_insensitive_get` bailed on any key
   without a `:`, so mainspace titles never got the first-letter fallback.

5. **`Templated table cell with untemplated attributes: Integrated mode only`**
   is in the categorically-unreachable group (see below), not a live gap.

### Parser functions: the `functionhooks` set

PHP resolves `{{#…}}` by looking the whole prefix up in the function-synonym
table, keyed with a `#` unless the name is in `$noHashFunctions`. rustoid sent
any `#…` straight to `broken: true` without consulting anything, so core's
no-hash functions could not resolve. `SiteConfig::function_hooks` now models the
API's `functionhooks` set.

Watch out: `dir` must be registered as a *function hook*, not a magic word.
Registering it as a magic word made `{{dir}}` resolve as a variable instead of
falling through to `Template:Dir` (there is a regression test for this).

`#dir` itself is only partly faithful. Core gets direction from
`Language::getDir()` and its `$rtlLanguages` table; rustoid has no language data,
and the direction is not decidable from the code alone (`ar` is RTL, `en` LTR).
Only the `-x-rtl` override is recognised, and an omitted code reports `auto`.
An earlier draft hardcoded a 24-entry RTL language list with `ar-`/`fa-`/`he-`
prefix heuristics — **no test exercises it and MediaWiki core is not in the
`/tmp/parsoid-src` checkout to port from**, so it was guesswork and was removed.
TODO: add per-language direction to `SiteConfig` (PHP's `languages` block has an
`rtl` flag per language) and look the code up.

## Next: a nested template in a template target

Five failures now share one root cause, and it is in the **tokenizer**.
`parse_template_token` takes the target as raw source text:

```rust
let target = parts.first().map(|(_, s)| strip_html_comments(s).trim()...)
```

PHP instead tokenizes `template_target` and keeps `{{…}}`/`{{{…}}}` as live
`template`/`templatearg` tokens, which expand in document order *before* the
outer target is resolved; `processToString` then flattens the buffer. Because
rustoid stringifies the target, an inner template is destroyed:

```
{{ {{T290526}} }}    →  <template T290526=""></template>   (PHP: expands, then {{ … }})
{{1x<invalid> |{{2x<invalid>y}}| }}  →  <template 2x<invalid>y=""></template>
```

Affected: `2. Using {{!}} in wikilinks`, `Using {{!}} in template arguments,
part 2`, and `Don't apply complex line-splitting heuristics in AttributeExpander
for non-<table> tokens` (the last one's bucket attribution is otherwise a red
herring — it is the same target bug, not line splitting). Also implicated in the
`T179544` `{{anchorencode:}}` family.

The fix is to give the target the same inline tokenization the argument values
already get (`try_wikilink`/template-arg tokens), and to resolve
`resolve_target_string` from tokens rather than a pre-flattened `String`. Note
that for `{{ {{T}} }}` the *expected* output keeps literal `{{`/`}}` around the
expanded `<span>` — the inner expansion is real, but it does not turn the outer
construct into a transclusion.

## Landed: the tokenizer's `preproc` stack (857 → 858)

PHP's grammar keeps a stack of pending closers, and
`TokenizerUtils::inlineBreaks` consults its top: `}}` breaks only while the
pending closer is `}}`, `]]` only while it is `]]`. A `[[` inside a template
argument therefore becomes the pending closer and *hides* the following `}}`,
which is the behaviour `Grammar.pegphp` documents at `broken_template`: once an
opener fails to close, the whole chain degrades to literal text, so
`{{1x|[[Foo}}` does not expand.

rustoid scanned for the closing braces with a plain brace counter, so it
expanded the template. `find_template_closing` now walks the pending-closer
stack the way `inlineBreaks` does. `{{1x|[http://example.com x}}` (a single
bracket opens an external link, not a wikilink) and `{{1x|[[a]]}}` (the
wikilink closes first) stay correct. Fixed
`Broken wikilinks (but not external links) prevent templates from closing`.

The same pass confirmed that `inlineBreaks` never consults the `extlink` stop
for `|`, so a pipe inside a single-bracket external link still separates
template arguments (`{{1x|[http://e.com |x]}}` passes `[http://e.com ` and
`x]`). That was the first of the three fixes that made
`Plain link in template argument` pass (see above).

## Landed: marker pairing across sibling subtrees (856 → 857)

A template that opens inside one element and closes inside a *sibling* element
was never encapsulated. `{|
| {{content and cell}}
|}` puts the
`mw:Transclusion` start meta in the first `<td>` and the `mw:Transclusion/End`
meta in the second; rustoid's range search only ever paired markers that were
themselves siblings (or reachable as siblings), so the start marker survived as
a stray `<meta>` inside the cell.

PHP's `DOMRangeBuilder::findWrappableTemplateRangesRecursive` pairs the metas
wherever they sit in the tree, and `findEnclosingRange` then lifts the range to
their common ancestor — the first cell becomes the encapsulation target and the
second only carries the `about` id.

rustoid now models that: `wrap_flipped_children` also recognises a child whose
*subtree* holds a start marker whose end marker lies outside that subtree, and
pairs it with the sibling holding the end marker. Fixed
`Newline constraint after multi-node template`.

## Landed: transclusion marker metas insert through InHead (854 → 855)

`RemexPipeline::insertUnfosteredMeta` calls `$this->dispatcher->inHead->startTag(...)`
— the InHead handler *directly*, not `dispatcher->startTag`. That distinction is
load-bearing: a transclusion marker meta emitted while a table row/cell is open
must stay where it is, whereas a normal `meta` start tag in a table context is
foster-parented out of the table. rustoid dispatched on the current insertion
mode, so the markers were hoisted out and ended up as siblings of `<table>`;
`wrapTransclusionChildren` then saw an adjacent start/end pair, treated the
transclusion as empty, and left an orphan `<span typeof="mw:Transclusion"></span>`
in front of the table.

Also ported the first branch of `DOMRangeBuilder::isDeletableNode`: a whitespace
text node in a *fosterable* position is always deletable, before any newline
analysis. Fixed
`2b: Delete whitespace/comments if found in fosterable position while
template-wrapping`, and moved `T343874` and `Cell combination tests` close to
passing (see the "Still open" list above).

## Landed: source tracking on `SourceRange` (850 → 854)

This was the "frame-source plumbing" item. Every `SourceRange` in the peg grammar
is built with `$this->source`, so a token produced by tokenizing a *template body*
indexes that body's text — which is why PHP's
`TokenStreamPatcher::convertNonHTMLTokenToString` can re-read a stray table tag's
wikitext no matter which source its caller holds (`SourceRange::substr` prefers the
range's own source over the passed string).

rustoid now models that: `SourceRange` gained an optional shared `source`, the
tokenizer stamps its input onto every `tsr` it produces, and `substr`/`substr_in`
prefer it. `substr` returns an owned `String` because the result may borrow from
the range rather than the caller's text.

With that in place, three faithful fixes landed:

1. **`TokenizerUtils::buildTableTokens`' attribute-source bookkeeping**,
   factored into `apply_build_table_tokens_attrs`. A cell whose attribute box
   parsed to nothing takes its `startTagSrc` from the tag character plus the box's
   whitespace and its `attrSepSrc` from the separator; `table_row_tag` records
   `attrSrc` from the end of the dashes to the end of the attribute box. A stray
   `|--|` row is therefore `startTagSrc = "|--"` plus `attrSrc = "|"`, so
   `|  |` and `|--|` survive as written instead of collapsing to `|` and `|-`.
   Fixed `Asciiart template should display the right number of spaces`.
2. **The extension-body sub-pipeline's `inTemplate` context.**
   `ParsoidExtensionAPI::wikitextToDOM` parses an extension body with
   `'inTemplate' => $this->inTemplate()` — false unless the extension was reached
   from inside a template. rustoid hardcoded `true`, which left templates inside a
   top-level `<spantag>` unencapsulated. Fixed
   `Multi-line extension tags should not interrupt table-cell parsing in the same
   row`.

### Still open in this area

- `T343874` now reaches `<table typeof="mw:Transclusion">` (the table-level
  encapsulation works). What remains is the cell-attribute reparse:
  `<td>class="test"<span typeof="mw:Transclusion">| foo</span></td>` should
  become `<td class="test">foo</td>` — i.e. the same
  `TableFixups::reparseTemplatedAttributes` work as "Cell combination tests".
- `Templated table cell with untemplated attributes: Cell combination tests` now
  produces all six cells with the correct kinds, attributes, and transclusion
  metadata. The remaining diff is a **stray `</tr>`**: the fixture's expected HTML
  has `<td …>…</td>\n<tr …>` where rustoid emits `<td …>…</td>\n</tr>\n<tr …>`.
  Since the harness normalizes both sides through an HTML parse, this is a real
  nesting difference, not a whitespace one — worth checking whether PHP's
  serializer omits an auto-inserted `</tr>` in that position.
- `Templated table cell with untemplated attributes: Integrated mode only` is one
  `about` attribute short of PHP's standalone output (see below), and its fixture
  expectation is `+integrated`-only (unreachable in standalone).

## Categorically unreachable: the `+integrated`-only table fixtures

`Templated table cell with untemplated attributes: Integrated mode only` and
`T343874` (plus the `Templated table cell with untemplated attributes` base
fixture, already recorded as a known failure) have **no `html/parsoid` or
`html/parsoid+standalone` section**. Their expected output is the
integrated-mode (production) single-cell / table-level-encapsulation form, which
PHP in standalone mode does not produce either — verified by running PHP's own
runner on a patched copy of `tables.txt`:

```
# row 1 of "Integrated mode only"
<td about="#mwt1" typeof="mw:Transclusion" class="foo">bar</td>
# row 2 (the `&nbsp;{{!}}` row) — note: NO typeof
<td about="#mwt3" class="foo">bar</td>
```

The fixture asks for `typeof="mw:Transclusion"` on row 2 as well. rustoid now
reaches PHP's standalone output for row 1 exactly; row 2 still drops
`about="#mwt3"` but is otherwise identical, i.e. it is one `about` attribute
short of *PHP*, and one `typeof` short of the *fixture*. Chasing the fixture
would be chasing an unreachable target; the faithful goal is PHP's standalone
output, and `about` on row 2 is the remaining gap.

The cause of the missing `about`: `mergeCells` sets `ignoreParsoidAttributes`
when the two cells have identical names (`td`→`td`), so `$from`'s `about` is not
copied onto `$to`. PHP's standalone `$to` (the second cell) already carried
`about="#mwt3"` from `tplwrap`, which is why PHP keeps it and rustoid does not:
rustoid's `tplwrap` does not encapsulate the inner split cell's transclusion
range. That is the same root cause as the `T343874` orphaned
`<span typeof="mw:Transclusion"></span>`: rustoid's `findEncapTarget` handling
for a table whose opening tag came from a template does not extend the
encapsulation over the `{{tbl-start}}`/`{{tbl-end}}` pair.

## Landed: argument-value expansion context (850 → 852)

`processTemplateSource` forwards the *caller's* `inTemplate` into the nested
expansion pipeline, while `AttributeTransformManager::process` expands the
template token's argument keys/values under a hard-coded
`[ 'expandTemplates' => false, 'inTemplate' => true ]` (TemplateHandler.php:988).
`{{!}}` therefore becomes a `<td>` cell token **only inside an argument value**,
and a literal `|` in the template body. rustoid expanded the whole spliced body
with a hard-coded `true`, which made `{{1x|1={{!}}bar}}` work but
`{{1x|1= {{!}}bar}}` diverge from `{{1x|{{T290526}}}}`. The template tokens
spliced in from an argument value are now marked
(`TempData::in_arg_value`) and `expand_templates` reads the mark back, so the
body expansion uses the caller's flag.

This **supersedes** the earlier "tempting fix that is wrong" note: the reason a
flat switch to the caller's flag regressed fixtures was that rustoid had no way
to distinguish argument-value tokens from body tokens. With the mark in place
the caller's flag is correct for the body *and* `inTemplate=true` still applies
where PHP applies it.

Also landed (both verified against `PegTokenizer::tokenizeTableCellAttributes`):

- `tokenize_table_cell_attributes` now requires the whole input to be consumed.
  `row_syntax_table_args` is a PEG *start rule*, so PHP's `tokenizeSync` reports a
  hard `false` on a partial match, and `TableFixups::reparseWithPreviousCell`
  treats that as "nothing to reparse".
- `{{…}}` is a valid table attribute *name* in cell-argument position. PHP's
  `table_attribute_name_piece` includes `directive`, and `inline_breaks` only
  stops a `{`-run at the cell separator `{{!}}`. This is what recovers
  `class="foo"` from `class="foo"{{1x|1=&nbsp;{{!}}bar}}|` — the piece that
  made `Integrated mode only` row 2 merge at all.

Fixed: `AttributeExpander regression test: Class attributes should be properly
applied`, `Multiple transclusions in discarded table attribute position should be
handled properly`.

## Landed: templated table-cell attributes (no count change; unblocks the cluster)

The `Templated table cell with untemplated attributes` cluster needed three faithful
fixes. All are verified against PHP's own `--dump=dom:post-tplwrap` output and
`PegTokenizer::tokenizeTableCellAttributes`:

1. **`{{{name|default}}}` must honour the default.** `Frame::expandTemplateArg`
   works off the token's *attribs* (`attribs[0].k` = name, `attribs[1]` = default);
   rustoid rebuilt the source as `{{{name}}}` at the call site, dropping the default,
   so a bare `{{{cmt|}}}` rendered as literal `{{cmt}}`. New
   `Frame::expand_template_arg_token` mirrors PHP exactly.
2. **A template body is tokenized once, at the body position.** rustoid re-tokenized
   the whole argument-spliced result at SOL, so `{{{attr|}}}{{{cmt|}}}| foo` had its
   `|` promoted to a cell separator and split one cell into `<th>`+`<td>`. Only a
   body made up *solely* of argument references needs the re-tokenize (there the
   substituted text really does land at the body start, which is what makes
   `{{1x|!!foo}}` a `<th>`).
3. **A lone `!` is not a table-attribute-name terminator.** `TokenizerUtils::
   inlineBreaks` breaks on `!` only as part of `!!`;
   `table_attribute_name_piece`'s fallback (`$( !(space_or_newline / [\0/=>]) . )`)
   accepts it as a one-character valueless attribute name. rustoid stopped on it, so
   `class="test"! align=left|` reparsed to nothing instead of `class`/`!`/`align`.

After these, all six cells of `Cell combination tests` reparse to the correct kinds
and attributes; what remains there is the `data-mw.parts`/`dsr` metadata detail
(the fixture's `html/php` section is the legacy parser's malformed output, so the
standalone/`+standalone` section is the one that matters).

`T343874` additionally needs transclusion wrapping of a table whose `{|`/`|}` come
from separate templates (`{{tbl-start}}`/`{{tbl-end}}`): rustoid currently emits an
orphaned empty `<span typeof="mw:Transclusion"></span>` before the table. PHP
attaches the transclusion to the `<table>` with a multi-part `data-mw`. (Note that
this fixture, like `Integrated mode only`, is `+integrated`-only — see the section
at the top of this file.)

### Superseded note (kept for the record)

An earlier session recorded that hardcoding `in_template: true` in
`expand_one_template`'s recursive `expand_templates` call was "suspicious" and that
switching it to the *caller's* `in_template` regressed two fixtures. That reading was
too coarse: the caller's flag **is** right for the template body, but argument values
need `inTemplate => true`. Marking the argument-value tokens
(`TempData::in_arg_value`) satisfies both, and `expand_one_template` now forwards the
caller's flag. See "Landed: argument-value expansion context" above.

## Superseded: "a tempting fix that is wrong"

**This section is obsolete.** Hardcoding `in_template: true` in
`expand_one_template`'s recursive `expand_templates` call was indeed wrong, and
switching to the caller's flag alone also regressed fixtures — but only because
rustoid could not tell argument-value tokens from body tokens. The
`TempData::in_arg_value` mark (commit `94782e7`) resolves that, and the caller's
flag is now forwarded. The text below is kept only to explain why the naive
version failed.

### A tempting fix that is wrong (recorded so it is not retried)

Hardcoding `in_template: true` in `expand_one_template`'s recursive
`expand_templates` call is suspicious (the comment on it is a TODO), and switching
it to the *caller's* `in_template` does fix `Integrated mode only` — `{{!}}` then
expands to the literal `|` instead of a `<td>`, so no spurious cell appears. It
regresses two fixtures, though:

- `Templated table cell with untemplated attributes: Regression tests`
- `Using {{!}} in template arguments (T290526)`

Both are **validated** by PHP's own runner (`php bin/parserTests.php --mock
--wt2html --filter '...'` passes them), whereas `Integrated mode only` is skipped by
that runner entirely — it is one of the three tests in its group that report
`0 of 0 tests matching`, so PHP never checks the fixture's `html/parsoid+integrated`
section. Matching the production path instead would be defensible in isolation, but
rustoid's fixture suite is generated from the parserTests configuration, so the
parserTests behaviour is the one that must hold.

Note when probing: `MockSiteConfig::getMagicWordMatcher` returns `/(?!)/` (never
matches) except for `toc`, so a standalone probe does **not** treat `{{!}}` as a
magic word unless the site config registers it (`--template` probes silently treat
it as a missing template). `bin/parserTests.php --filter` matches on the test name
as a plain substring and silently reports `0 of 0 tests matching` when the name
does not start the matched list, which makes a wrong name look like a passing run.

## Landed: autolink URL scanning stops at table pipes; `cleanUrl` wikilink mode (848 → 849)

Closed "Table security: embedded pipes" (T3830-adjacent). Three faithful-port fixes in
the URL/`urllink` tokenizer path:

1. **`no_punctuation_char`** (`Grammar.pegphp:798`): the `url`/`autourl` `path` is a run
   of `!inline_breaks @(no_punctuation_char / …)`. rustoid's `try_urltext` used an ad-hoc
   stop set; the real class is `[ \]\[\r\n\"'<>\x00-\x20\x7f&\u00A0\u1680\u180E
   \u2000-\u200A\u202F\u205F\u3000{]`. Note `|` is **not** excluded.
2. **`inline_breaks`'s `$stops['table']` branch**: in a table context a `|` followed by
   `|` or `}` *does* break, which is what stops `[ftp://|x||]` at `ftp://|x` instead of
   swallowing both pipes. Applied in both `try_urltext` and `scan_extlink_url_len`.
3. **`extlink` fails when its content starts with a break character**: the grammar is
   `"[" url (space)* inlineline<extlink>? "]"`, and `inlineline` stops at an
   `inline_breaks` position. When the first content char breaks, `inlineline` is empty and
   the required `]` is not there, so the whole rule fails — the `[` becomes literal text
   and the URL is re-scanned as an autolink (PHP's token stream confirms: a `urllink`, not
   an `extlink`).
4. **Autolink text uses `cleanUrl` in `wikilink` mode** (`ExternalLinkHandler::onUrlLink`
   ends with `Sanitizer::cleanUrl( …, $href, 'wikilink' )`). Only the `external` mode
   percent-encodes, so the displayed text keeps `ftp://|x` while the `href` attribute
   carries `ftp://%7Cx`. `on_url_link` now takes a second cleaner.

Also landed in this commit: the `cellAttrTerminatorSeen` plumbing
(`AttributeExpander::stripMetaTags` → `dp.tmp` → `TableFixups::handleTableCellTemplates`),
which is correct but does not by itself fix a fixture yet.

## Landed: extensions tunnelled through DOM fragments + TT2 attribute order (847 → 848)

Closed "Table cell attributes: Pipes protected by nowikis should be treated as a
plain character" (T280115) by making two structural fixes that PHP already has:

1. **`nowiki` now tunnels through a DOM fragment**, as PHP's
   `ExtensionHandler::onExtension` does for *every* extension via
   `PipelineUtils::tunnelDOMThroughTokens`. The token stream sees a real
   `<span typeof="mw:DOMFragment" data-fragment-id=…>` start-tag token — a
   shallow clone of the fragment's own first node, which is what
   `getWrapperTokens` builds (`wrapperName` for a lone inline element is that
   element's own name). Emitting an opaque `mw:dom-fragment-token` meta instead
   is **wrong**: `paragraph_wrapper_v2.rs` treats that name as SOL-transparent
   and swallows it, so `<nowiki>…</nowiki>` on its own line stopped being
   p-wrapped (7 regressions; the real span tag fixed 6 of them).
2. **The ExtensionHandler now runs inside attribute values, before the
   AttributeExpander.** PHP's `AttributeExpander` expands a value with
   `Frame::expand`, which routes it through the full TT2 chain
   (`TemplateHandler → ExtensionHandler → AttributeExpander`); rustoid had the
   ExtensionHandler only in `TreeBuilderStage::process`, i.e. after attribute
   expansion. New `extension_handler::expand_in_attributes` reproduces the TT2
   order, so `stripMetaTags` sees the `mw:DOMFragment` type and sets
   `hasGeneratedContent` → `typeof="mw:ExpandedAttrs"`.

Supporting faithful ports needed to make the value survive to the DOM:

- **`tokensToString`'s `unpackDOMFragments` option** (`TokensToStringOpts`): a
  `mw:DOMFragment` placeholder resolves to its fragment's *text content*, not its
  HTML. PHP's comment explains why: the correct thing would be
  `innerHTML`, but `<translate>`/`<nowiki>` are the expected contents and
  `textContent` will do. This is what turns `foo` + fragment(`|`) into `foo|`.
- **`Sanitizer::sanitizeTagAttrs` uses `fetchExpandedAttrValue($k)->textContent`**
  for token-array values, rather than stringifying them. rustoid's cell
  sanitization was flattening the `Tokens` value (dropping the fragment) before
  the DOM saw it.
- **`data-mw` is node data, not an attribute.** PHP keeps it on the token's
  `DataMw` object; rustoid models it as a string attribute, so it must be pulled
  out *before* the cell sanitizer runs (which would drop it as a reserved
  `data-` attribute).
- **Templated attribute keys are no longer dropped** from the element's plain
  attributes. PHP's `TreeBuilder/Attributes.php` keeps every token attribute on
  the element; `data-mw.attribs` is metadata about how they were produced, not a
  replacement for them. The old `templated_attrib_keys` filter was removing the
  flat `title="foo|"`.
- **`nowiki_items_to_fragment`** builds the stashed sub-`Node`
  (`<span typeof="mw:Nowiki">` + `mw:Entity` children) from the token sequence,
  mirroring `Nowiki::sourceToDom` + `convertDOMtoTokens`.

Also fixed en route: `bail_dirty_redirect` registered its fragments in a **local**
map, so the emitted `mw:DOMFragment` placeholder had a dangling id and the
`<nowiki>` content vanished (`REDIRECT [[]]`). It now uses the caller's map.

## Landed: table-cell attribute values keep their tokens (847, no count change)

Groundwork for the `TableFixups`/`AttributeExpander` cluster, from
"Table cell attributes: Pipes protected by nowikis should be treated as a plain
character" (T280115). Two faithful-port fixes, both prerequisite to the rest:

1. **`scan_quoted_table_value_end` did not skip directives.** PHP builds a
   quoted table-attribute value with `table_attribute_preprocessor_text_double`,
   whose alternatives route `{ } & <` through `directive` — so
   `<nowiki>|</nowiki>` is consumed as **one unit** and the `|` inside it does not
   terminate the value. rustoid did a raw byte scan, so
   `title="foo<nowiki>|</nowiki>"` was cut at the inner pipe and the cell fell
   back to literal text. The scan now walks directives using the non-emitting
   primitives (`parse_extension_tag`, `parse_html_entity`, `skip_template_or_tplarg`).
   Note the `|` stop is context-dependent (`cell_arg`): in start/row-tag position
   a bare `|` is ordinary value content, which is why `style="a|b"` keeps its
   pipe; `{{!}}` is a stop in both positions.
2. **`parse_table_att_value` returned a `String`,** discarding the token array
   that `getAttrVal` preserves in PHP. It now returns a `KeyValue` via the new
   `tokenize_table_value`, so a `<nowiki>` in a cell attribute survives as an
   `extension` token instead of being flattened to text. Values with no
   directives still come back as `KeyValue::Str`, so the fast path is unchanged.

**Still open for this fixture:** the `mw:ExpandedAttrs` marker — root cause now
definitively identified, with a concrete plan below.

`ExtensionHandler::onExtension` (PHP) **always** ends with
`PipelineUtils::tunnelDOMThroughTokens` — including for `<nowiki>`, which is only
special-cased in the block *before* it (skipping the `about`/`data-mw`/
`mw:Extension/<name>` wrapper; the comment says "Parsoid has treated <nowiki>s as
core functionality with lean markup"). The wrapper token that
`tunnelDOMThroughTokens` builds always carries `typeof="mw:DOMFragment"`.

rustoid deliberately skips that indirection — `nowiki_items` builds the
`<span typeof="mw:Nowiki">` tokens directly ("without the DOM indirection").
That is invisible on the page path, but inside an attribute value it loses the
signal `stripMetaTags` reads via `hasDOMFragmentType` to set
`hasGeneratedContent`, so the attribute is never marked `mw:ExpandedAttrs`.
Verified by instrumenting `strip_meta_tags`, which sees
`[Str("foo"), Tag(span, typeof=mw:Nowiki), Str("|"), EndTag(span)]` where PHP's
token pipeline sees a `mw:DOMFragment` wrapper.
Note `mw:Extension` is **not** in PHP's `META_TYPE_MATCHER`
(`mw:(LanguageVariant|Transclusion|Param|Includes|Annotation/)`), so the
`wrapTemplates` branch does not fire either — it really is the DOM-fragment path.

### Two narrower attempts failed (kept for the record)

Each fixed the target cell while regressing 6 fixtures, because the value was
*already rendered* and must not be re-expanded:

- running `extension_handler::run` over every `Tokens` attribute value inside
  `expand_attributes`;
- the same, scoped to values containing an unexpanded `extension` token.

The regressions were `<nowiki> inside a link`, `<pre> inside a link`, "Nowiki
markup in link attribute (T206940)", "T107474: Frameless image caption with
nowiki", "T374445: Non extlink in media caption", "3. Other redirect variants".

**Resolved by the landed change above** (see the top section): the fix was not to
re-expand values but to run the ExtensionHandler in the correct TT2 position, and
resolve the fragment only at the points PHP does (`tokensToString` with
`unpackDOMFragments`, and `sanitizeTagAttrs`'s `fetchExpandedAttrValue`).

## Landed: html2wt link target handling (846 → 847)

Fixed "Parsoid T55221: Wikilinks should be properly entity-escaped". Four
separate faithful-port gaps, all in `Html2Wt/LinkHandlerUtils.php` and
`WikitextSerializer.php`:

1. **`serializeText` must call `Utils::escapeWtEntities`.** PHP does
   `if ( $state->needsEscaping ) { $res = Utils::escapeWtEntities( $res ); }`
   *before* `emitChunk`, so a DOM text node holding the literal characters
   `He&nbsp;llo` (which is what an HTML5 parse of `He&amp;nbsp;llo` yields — the
   entity is **not** decoded) serializes back as `He&amp;nbsp;llo`. rustoid's
   `serialize_text` emitted the raw text.
2. **`normalizedTitleKey` must surface `TitleException`.** PHP's
   `makeTitleFromURLDecodedStr` goes through `Title::newFromText`, which throws
   for illegal title characters; `isValidLinkTarget` passes `noExceptions` and
   gets `null`. rustoid used the infallible `TitleParser::parse`, so
   `He&nbsp;llo` (containing `&`) was wrongly considered a valid target. Now uses
   `TitleParser::try_parse`, and resolves relative `#`/`/`/`../` references first,
   as PHP's `makeTitle` does.
3. **The simple-link branch must record `escaped_tgt`.** PHP assigns
   `$escapedTgt` in that branch too; the invalid-link fallback at the end of
   `serializeAsWikiLink` keys off it. rustoid assigned it only in the piped
   branch, so an invalid target was never downgraded to plain text — and PHP logs
   `Bad title text` and emits just the content.
4. **The piped branch was missing its target resolution.** PHP strips the
   relative prefix (`preg_replace( '#^(\.\.?/)*#', '', ... )`), replaces `_`
   with a space unless the content is relative or the link is interwiki, then
   escapes and (when valid or not from source) applies `addColonEscape`. rustoid
   only escaped, so `[[Foo|…]]` came out as `[[./Foo|…]]`.

## Resolved: `Mixed Lists: Test 11` (T175099) — 845 → 846

The mystery from the previous session is solved, and the earlier analysis was
wrong in an instructive way.

**What was actually happening.** The fixture has both an `html/php` and an
`html/parsoid` section:

```
;a
:*b
!! html/php      <dl><dt>a</dt>\n<dd>\n<ul><li>b</li></ul></dd></dl>
!! html/parsoid  <dl><dt>a\n<dd><ul><li>b</li></ul></dd></dl>
```

Parsoid's parser genuinely produces the **flat** form `<dt>a</dt>\n<dd>…` — the
same thing rustoid produces. Confirmed two ways:

- `php bin/parserTests.php --mock --wt2html --dump=dom:post-pwrap --filter …`:
  `<dl…><dt…>a</dt>\n<dd…><ul…><li…>b</li></ul></dd></dl>`.
- The `/tmp/pt_fixture.php` probe (which the previous session distrusted
  correctly).

And the test still passes, because the *expected* side is normalized the same
way. `Test::normalizeHTML` picks `parsoidOnly = true` (an `html/parsoid` section
exists), so the expected goes through `TestUtils::normalizeOut`, which re-parses
the HTML with **`DOMUtils::parseHTML`** — an HTML5 parse. HTML5's "in body"
implied-end-tag rule closes the `<dt>` when the `<dd>` start tag arrives, so the
hand-written unbalanced markup normalizes to exactly the flat form.

So both sides converge; the bug was in rustoid's **harness**, not the parser.

### The fix

The harness used a hand-rolled string walker (`parse_fragment`) to build its
`MNode` tree, and that walker treated `<dl><dt>a` then `<dd>` as *nesting* — the
`<dd>` became a child of the still-open `<dt>`, with `</dt>` closing last. Added
`closes_open_element`, implementing the HTML5 implied-end-tag pairs that occur in
unbalanced hand-authored parser-test HTML: `dd`/`dt` → closes an open `dd`/`dt`,
`li` → closes `li`, `h1`…`h6` → closes a heading. The walker now pops out of the
recursion and lets the caller re-consume the tag, mirroring `parseHTML`.

Note this is purely a test-harness fidelity issue: `rustoid_core`'s own
`html::parse::parse_html` already handled the unbalanced markup correctly (it uses
`html5ever`).

## Landed: `:last-child` in the test-harness selector (843 → 845)

`matches_pseudo` had `:last-child` stubbed as `one_based == 1` (i.e. identical to
`:first-child`), so `["td:last-child", "after", …]` matched the *first* cell and
the pasted cell landed at the front of the row.

The sibling index is now a small `Sib { index, total }` carrying both the
0-based element-sibling position and the total element-sibling count, threaded
through `walk`/`matches_full_selector`/`matches_compound`/`matches_pseudo`, so
`:last-child` is `index + 1 == total`.

Fixed: "T319143 - copy-pasting of cells, after multiple cells" (both variants).

## Landed: `DisplaySpace` (842 → 843)

Ported `src/Wt2Html/DOM/Handlers/DisplaySpace.php` as
`rustoid-core/src/pipeline/display_space.rs` and wired it into the main DOM
pipeline after `table_fixups`/`linkneighbours` and before `cleanup` (matching
`displayspace` in `FULL_PARSE_GLOBAL_DOM_TRANSFORMS`).

It applies French-space armoring: a space before `? : ; ! % » ›` (when not
followed by a word char), or a space after `« ‹` (when not preceded by one), is
replaced by a non-breaking `mw:DisplaySpace` span. `<pre>`, `<svg>`, and raw-text
elements are skipped, matching the `textHandler` early return.

Two harness gaps had to be closed for the fixture to compare equal:

- `TestUtils::unwrapSpansAndNormalizeIEW`'s `stripSpanTypeof` unwrapping was not
  ported. Marker spans (`mw:DisplaySpace`, `mw:Placeholder`, `mw:Nowiki`,
  `mw:Transclusion`, `mw:Entity` — or only `mw:Placeholder` for a Parsoid-only
  test) are now reduced to their inner HTML.
- That unwrapping must run **before** the attribute-stripping passes, since it
  keys off `typeof`. It is therefore a string pass over the raw HTML rather than
  a post-parse tree pass.

Fixed: "Definition lists: ignore colons inside tags".

## Landed: quotes in link content + `stringifyOptionTokens` (841 → 842)

### 1. The `quote` rule inside link content

PHP's `extlink` content is `inlineline<extlink>?` and wikilink text is
`link_text`, so `[http://wp.org ''foo'']` yields an `mw-quote` token that the
QuoteTransformer turns into `<i>`. rustoid only tokenized *directives*
(`{{…}}`, `{{{|}}`, extension tags, lang variants) in link content, so the
apostrophes stayed literal text.

- `tokenize_link_content` now also runs the `quote` rule (`try_quote`).
- `tokenize_link_target` deliberately does **not**: PHP's
  `wikilink_preprocessor_text` (the target rule) has no quote production.
  Applying quotes there broke four media/attribute fixtures, so the flag is
  split between the two entry points.
- Added `PegTokenizer::output_len`/`drain_output` so a sub-rule's tokens can be
  captured without disturbing the surrounding buffer.

### 2. `stringifyOptionTokens`' `mw-quote` bail-out

A quote run contributes no text, so `[[File:Foobar.jpg|'''thumb''']]`
stringified its caption to `thumb` and the option was misread as the `thumb`
format — producing a `<figure>`/`<figcaption>` instead of a plain `<span>`.

PHP's `stringifyOptionTokens` returns `null` for an `mw-quote` unless the text
so far resolves to a `link`/`alt` option (the two options allowed arbitrary
wikitext); a `null` means "this is a caption". Ported as
`stringify_option_tokens` and used for the `mw:maybeContent` part in
`render_file`; a `None` now routes the part to the caption path with its raw
tokens.

Fixed: "Parsoid-centric test: Whitespace in ext- and wiki-links should be
preserved", "Media with caption that would stringify to a valid media option".

## Landed: the list/SOL cluster (840 → 841)

### `table_end_tag` is not at SOL

`try_table_end_tag` set `at_sol = true` after emitting the `</table>`
end tag. But `|}` consumes no newline, so the position after it is *not* a line
start — PHP reaches `table_line` only through `sol block_line`, which has already
consumed the leading newline. Because of that stale `at_sol`, the following
`<!-- bar -->` was greedily wrapped into an `EmptyLineTk` (rustoid's step "2a"
`empty_lines_with_comments` call), whereas PHP emits a plain `CommentTk` +
`NlTk`.

The `NlTk` matters: in the ListHandler it sets `at_eol`, so the next
non-SOL-transparent token (`this text`) triggers `closeLists`. With the comment
swallowed into an `EmptyLineTk` that never happened, and the paragraph was
absorbed into the `<dd>`.

- `try_table_end_tag` now leaves `at_sol = false`.
- The step-2a `empty_lines_with_comments` call is now gated on `self.at_sol`,
  matching PHP's grammar (the rule only runs inside `sol`).

Fixed: "Hacky use to indent tables, with comments (T65979)".

## Landed: the selser `T319143` cluster (834 → 840)

The whole 8-fixture "T319143 - copy-pasting of cells" group shared **one** root
### 1. `Test::applyManualChanges` — the `before`/`after`/`append` fragment context

PHP's `$jquery['before'|'after']` closures do **not** parse the inserted HTML
generically. When the target's parent is a `<tr>` they set the inner HTML of a
scratch `<tr>` of a scratch `<table>`:

```php
} elseif ( DOMUtils::nodeName( $node->parentNode ) === 'tr' ) {
    $tbl = $node->ownerDocument->createElement( 'table' );
    DOMCompat::setInnerHTML( $tbl, '<tbody><tr></tr></tbody>' );
    $tr = $tbl->firstChild->firstChild;
    DOMCompat::setInnerHTML( $tr, $html );
```

That matters because HTML5's "in body" insertion mode **drops** `<td>`/`<tr>`/
`<tbody>` start tags outright, so parsing `<td>…</td>` in a `<div>` context
loses the `<td>` (verified against remex: `<a href="./Eau">Eau</a>` alone).
rustoid previously dropped it too, so the inserted cell vanished.

- Added `html::parse::parse_fragment_in_context(html, context)` — a faithful
  `DOMCompat::setInnerHTML`, using `html5ever::parse_fragment` with an explicit
  context element and descending through the synthetic `<html>` wrapper.
- `apply_insertion` now re-parses per target with the right context: `tbody`
  parent → `<table>`; `tr` parent → `<tr>`; `append` on a `<tr>` → `<table>`’s
  `<tbody>` children; otherwise `<div>`. It also honours the `tbody` parent case
  for `before`/`after`, which was previously unimplemented.

### 2. `ContentModelHandler::canonicalizeDOM` — `RemoveRedLinks`

MediaWiki renders a link to a non-existent page with a `?action=edit&redlink=1`
query and a `new` class. Parsoid strips those two parameters before diffing, or
every red link compares as modified. Ported as
`html::remove_red_links::remove_red_links` (faithful to
`src/Html2Wt/RemoveRedLinks.php`, including parameter-order preservation and the
"empty query means no `?`" rule). Applied in `selective_serialize_dom` to **both**
the edited DOM (PHP `fromDOM`) and the revision DOM (PHP `setupSelser`).

### 3. `LinkHandlerUtils` — two missing gates

- `isSimpleWikiLink`: the guard is
  `!empty($target['modified']) || !empty($linkData->contentModified) || $dp->stx !== 'piped'`.
  `contentModified` was missing, so freshly-inserted links always got the piped
  `[[Eau|Eau]]` form.
- `linkData.contentModified` was never populated at all. Now set in
  `getLinkRoundTripData` from `state.inInsertedContent ||
  DiffUtils::hasDiffMark($node, SUBTREE_CHANGED)`; `in_inserted_content` is now
  save/set/restore threaded around the handler dispatch in `serializer.rs`
  (faithful to `WikitextSerializer::serializeDOM`).
- `getContentString`: diff markers
  (`<meta typeof="mw:DiffMarker/…">`) must be **skipped**, not treated as
  non-text. The old code returned `None`, so an inserted `<a>` whose children
  were `[marker, text, marker]` never produced a content string.

### Also fixed this session (from the previous handoff's diagnosis)

- `Test::applyManualChanges` `remove` with an optional selector: text nodes are
  removed unconditionally (PHP's "text node hack!"); only elements are filtered
  by the nested `querySelectorAll`.
- `find_matches` now includes the **root** when the rightmost compound matches,
  matching PHP's `DOMCompat::querySelectorAll` (not JS `Element.querySelectorAll`).

## PHP reference checkout — restored and working

The reference Parsoid checkout at `/tmp/parsoid-src` (commit
`d79c17f03af7423c7c2dcc73d25a6f63a4b805e2`) **works again as of this session**. It
had lost `.git/HEAD`, `.git/config`, and the ref files, so git refused to read
it and ~83 source files were missing. They were recreated:

- `.git/HEAD` → `d79c17f03af7423c7c2dcc73d25a6f63a4b805e2`
- `.git/config` with the `blob:none` partial-clone remote + `extensions.partialClone`
- `.git/refs/heads/master`, `.git/refs/remotes/origin/{HEAD,master}`
- then `git checkout -- .` restored the missing files

The tree is a **blobless clone**, so `.git/log`/`git log` still fails on missing
parent commits — that is expected and harmless. Working tree is clean.

Verify it in one command:
```bash
cd /tmp/parsoid-src && php /tmp/pt_single.php '[[Foo|bar]]'
```

### Probes
- `/tmp/pt_single.php '<wikitext>'` — bare standalone parse (MockDataAccess,
  no templates).
- `/tmp/pt_fixture.php '<wikitext>' [--template Name=body]... [--title T]` —
  standalone parse with **template fetching** (the primary wikilink/template
  oracle).
- `/tmp/pt_subpage.php '<wikitext>' [title]` — standalone parse with subpages
  enabled for NS 0 (the `subpage` test option).
- `/tmp/pt_html2wt.php '<html>'` — html2wt via `WikitextSerializer::serializeDOM`.
  Good for quick link-serialization checks, but it does **not** run
  `prepareAndLoadDoc`/`fromDOM`; use `/tmp/pt_h2w3.php` (below) for the full
  pipeline.
- `/tmp/pt_h2w3.php '<html>'` (**added this session**) — the faithful html2wt path:
  `DOMUtils::parseHTML` → `DOMDataUtils::prepareAndLoadDoc` → `env->setupTopLevelDoc`
  → `ContentModelHandler::fromDOM`. This is what the html2wt fixtures exercise, and
  it differs from plain `serializeDOM` (e.g. link-prefix `<nowiki/>` escaping only
  appears here). `PT_LANG=is` selects Icelandic link trail/prefix regexes.
- Known probe limitation: `MockSiteConfig` registers no magic words, so `{{!}}`
  does **not** resolve as a variable. Use the fixture runner for `{{!}}` cases.

## Landed this session

Eleven focused, PHP-verified fixes (821 → 833). See the commits below.

### 11. `textCanParseAsLink` link-validity walk (`0ec8676`)
- The trailing-bracket strip used the *first* `]`; PHP's
  `preg_replace( '/\][^\]]*$/D', ']', $text, 1 )` collapses the *last* `]` plus
  any following non-`]` characters. `]]` was reduced to `]` and wrongly nowiked.
- The link-validity walk was missing: PHP walks the tokens backwards, accepts a
  `wikilink` whose `href` is a valid local target (and not a bare protocol URL),
  handles `extlink` (including "template expands to a url link"), and otherwise
  accumulates token source to test whether `text` emerged unscathed.
- Fixed: "Parsoid link bracket escaping".

### 10. `WikiLinkText` chunks on the non-selser path (`2ba683e`)
- PHP's `serializeAsWikiLink` always wraps its output in `new WikiLinkText(...)`;
  the `stx in {simple,piped}` guard lives only in `fromSelSerImpl` (selser).
  rustoid passed `bad_prefix: None`, so no link-prefix `<nowiki/>` was installed.
- Added `constrained_text::wiki_link_with_config` (faithful `WikiLinkText`
  constructor) and refactored `from_wiki_link_chunk` onto it.
- Fixed: "Parsoid link prefix escaping".

### 9. html2wt metadata + relative titles + test options (`3194f28`)
- `parse_html` never decoded the `data-parsoid` attribute into `node.dp`, so every
  `getShadowInfo` lookup saw no `a`/`sa` shadow map. Added
  `DataParsoid::from_data_parsoid_json` (the inverse of `to_data_parsoid_json` for
  the html2wt-relevant fields), mirroring `DOMDataUtils::loadDataAttribs`.
- Ported `Env::resolveTitle` in full (was a lone-fragment stub): `(../)+` relative
  subpage resolution, absolute `/subpage`, trailing-slash trimming, re-normalization.
- The wt2wt harness path ignored the test's `subpage`/`language`/`!! config` options
  and passed no context title; added `Parser::wikitext_to_ast_with_title` and a
  shared `apply_config_raw` helper.
- Fixed: "Relative subpage noslash link".

### 8. `isNewElt`: no `data-parsoid` means new (`fbc73e2`)
- `DataParsoid::defaultValue()` sets `IS_NEW`, so `WTUtils::isNewElt` is true for
  HTML parsed without metadata. `node_is_new` was a stub returning `false`.
- Fixed: "Parsoid link trail escaping".

### 7. `isSimpleWikiLink` + `getFullDBKey` (`74295ce`)
- Ported PHP's missing `isSimpleWikiLink` branches: the
  `normalizedTitleKey(...) == preg_replace(MW_TITLE_WHITESPACE_RE, '_', target)`
  comparison, the relative-link `resolveTitle`/`../`-stripping branches, the
  protocol-relative `hrefHasProto` guard, and the interwiki colon-escape strip.
- Split `Title::getPrefixedDBKey` (no fragment) from `getFullDBKey` (appends it).
  Three call sites were hand-appending the fragment and produced `#section#section`.
- Normalize the fragment's whitespace at parse time (MediaWiki runs the same
  whitespace regex over the whole input), so `[[Foo#a&#160;b]]` gives `Foo#a_b`.
- Fixed: "Link containing % as a single hex sequence interpreted to char".

### 6. Wikilink link text tunnelled through a DOM fragment (`83eef20`)
- `render_wiki_link_dispatched` now passes the caption fragment builder (and the
  fragment/id maps) to `render_wiki_link_with_fragment`, which registers the
  built subtree via `dom_fragment_token`. Mirrors PHP's `renderWikiLink` calling
  `addLinkAttributesAndGetContent(..., $buildDOMFragment = true)`.
- Fixed: "<pre> inside a link".

### 5. Extlink URL scan (`cc2424a`)
- New `scan_extlink_url_len` implements PHP's `extlink_nonipv6url`: stops at
  `[`, `<`, `]`, `}`, quotes, whitespace, and directive/entity starts; continues
  through `|`, `&`, `=`, `-`, `!`, `{`.
- Fixed the close-bracket arithmetic (`rem` starts after `[`, so the advance
  must not re-add it), preserving the trailing `]` of `[http://x]]`.
- Fixed: "Nested wikilink syntax in wikilink syntax that parses as wikilink in
  extlink".

### 3. Entities in HTML attributes + real nested-link detection (`0d63203`)
- `parse_html_entity` extracted (non-emitting) and called from
  `parse_attr_value_text` too: PHP's `attribute_preprocessor_text*` route `&`
  through `directive`, producing an `mw:Entity` span, so the value decodes
  (T72875's `&#91;&#91;` case).
- `contains_toplevel_wikilink_open` replaces the raw `contains("[[")` test for
  the `Link-in-link` bail; it skips `<nowiki>`, recognized HTML tags, templates,
  and language variants. PHP throws only on a real nested `a rel="mw:WikiLink"`.
- Fixed: "T72875: Test for brackets in attributes of elements in internal link
  texts".

### 2. `|` in HTML attribute values is non-structural (`9420ccb`)
- New `skip_recognized_html_tag`; used in the `[[…]]` close scan and in
  `split_template_args_impl`, so `<span class="a|b">` does not split link
  content or close the link. Unrecognized tag names stay plain text.
- `split_template_args_impl` rewritten to index by byte (it previously mixed
  `chars[]` lookups with `&str` slices, mis-slicing multibyte input).
- Fixed: "Pipe in html attribute is link description".

### 1. Invalid template targets bail to literal text (`0b23b0a`)
- `TitleParser::try_parse` mirrors `Title::newFromText`'s `TitleException` checks
  (illegal chars, `%hh`, `&name;`, relative path components, `~~~`, over-long,
  empty) and returns `None` where PHP throws. `parse` stays infallible.
- `resolve_target_string` uses it, matching PHP's
  `makeTitleFromURLDecodedStr(..., $noExceptions = true)`.
- `TemplateHandler::convert_to_string` re-emits the literal `{{` … `}}` around
  the re-tokenized inner source (PHP `convertToString`).
- Fixed: "Ensure that transclusion titles are not url-decoded",
  "Wikilinks with embedded newlines are not broken".

### Next candidates
- "Parsoid-centric test: Whitespace in ext- and wiki-links should be preserved" —
  `[http://wp.org ''foo'']` should produce `<i>foo</i>`, but `tokenize_link_content`
  only tokenizes *directives* (`{{…}}`, extension tags), not quotes/entities, so the
  content stays a plain string and the QuoteTransformer never sees it. PHP's
  `extlink` content is `inlineline<extlink>` and does produce `mw-quote`.

  ⚠️ Tried: switching `tokenize_link_content` to a full `try_inline_element` walk with
  `linkdesc: true`. That regressed 833 → 814 — the tests
  (`test_extlink_url_stops_at_nested_wikilink`, `test_extlink_nested_wikilink_content`,
  `test_wikilink_close_ignores_html_attr_pipe`) depend on link content keeping a raw
  `[[…]]` as literal text, and `linkdesc` alters `[` handling. Reverted. A correct
  fix must keep the nested-`[[` invariant while still producing quote tokens —
  likely by tokenizing only quotes/entities inline and leaving brackets alone.
- "Parsoid T55221: Wikilinks should be properly entity-escaped" —
  `He&amp;nbsp;llo [[Foo|He&amp;nbsp;llo]]` should keep `&nbsp;` escaped and drop the
  `./` prefix (`[[Foo|…]]` not `[[./Foo|…]]`).
- "T179544: {{anchorencode:}} output should be always usable in links" — needs the
  `anchorencode` parser function plus `mw:ExpandedAttrs` on a templated wikilink
  fragment (AttributeExpander cluster).
- "T45661: Piped links with identical prefixes" — red-link mock/harness default;
  PHP's standalone known-failure for `html2wt` shows the piped form.

The table-cell / `TableFixups` cluster (14 fixtures) and the selser `T319143` group
(8 fixtures) are the largest remaining blocks; both need the AttributeExpander /
TableFixups work described below.

## Previous session notes

### Token-level template-argument expansion

> **Token-level template-argument expansion is LANDED and now net-neutral** (`40afb43`, `cdeef5b`,
> `3d82231`). The token-level path produces byte-identical behavior to the old string path across all
> 891 fixtures (fixture pass count back to **821**, exact same failing set as baseline). This lays the
> correct foundation for the table-cell cluster.

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

**Still failing** — table-cell template cluster (updated):
- "4. Template-generated table cell attributes and cell content inside a templated table"
  (`{{tbl-start}}…{{tbl-end}}` wraps the whole `<table>`; needs `typeof` on `<table>`, not an empty span)
- "Templated table cell with untemplated attributes" ("Cell combination tests") and its
  `+integrated`-only variants ("Integrated mode only", T343874 — see the note at the top of this file
  for why those expectations are unreachable)
- ~~"Multiple transclusions in discarded table attribute position should be handled properly"~~
  **fixed** by the argument-value expansion-context change (`{{…}}` as a table attribute name).
- The merge-cell path's DSR/source-recovery and data-mw bookkeeping still needs refinement so a
  merged cell's `about`/`typeof`/`data-mw` match PHP byte-for-byte (the remaining gap for the
  `&nbsp;` row).

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
`<td class="foo">title="fail"|bar</td>` with `data-mw.parts=["|class=\"foo\"",{template}]`.

**Foundation now LANDED** (token-level arg expansion, net-neutral): `{{1x|1={{!}}title="fail"{{!}}bar}}`
now expands `{{{1}}}` → `[template(!), 'title="fail"', template(!), 'bar']`, and `{{!}}` →
`<td attr_src='' at_src_start>` (via `process_special_magic_word`, `inTemplate=true`), so the cell content
is now `<td>class="foo"title="fail"</td>` (current) — `title="fail"` is *preserved* (it was dropped before),
but the inner `<td>` markers from `{{!}}` are not yet re-interpreted as cell separators, and `class="foo"`
is not yet reparsed as an attribute.

**Remaining piece (precisely):** the two `{{!}}`-produced `<td attr_src='' at_src_start>` markers land as
*inner* `<td>` elements inside the outer cell, and `TableFixups` must re-interpret each as a cell separator —
collect the `class="foo"title="fail"` text + inner-`<td>` markers, re-tokenize the leading `class="foo"` as
`row_syntax_table_args` attributes, hoist `typeof`/`about`/`data-mw`, and drop the consumed prefix — the same
`collect_attributish_content`/`reparse_templated_attributes` machinery already ported, but it must be driven
for the *inner-`<td>`-marker* case (not just the plain `k=v|` text case). PHP's `processSpecialMagicWord` +
`TableFixups::handleTableCellTemplates` do exactly this (`{{!}}` → `<td attrSrc='' AT_SRC_START>` → the
cell-template handler sees the `<td>` and reparses).

**Tree-builder split confirmed working** (this turn): the `{{!}}`-produced `<td attr_src='' at_src_start>`
markers *do* reach the tree builder in `InCell` mode (`[TBD] … mode=InCell`), and `modes::in_cell::start_tag`
for `td` fires `close_the_cell` + `in_row` reinsert (`td_in_scope=true`), so the cell splits into three `<td>`s
(`class="foo"` / `title="fail"` / `bar`). The remaining deficiency is **in `TableFixups`'s merge**, not the
split: `reparse_with_previous_cell` / `reparse_templated_attributes` must merge those three cells into the single
`<td class="foo">title="fail"|bar</td>` with `data-mw.parts=["|class=\"foo\"", {template}]` — currently
`class="foo"` is not recovered as an attribute and the trailing `bar` cell is lost.

**Diagnosed turn-by-turn (precise root cause):**
1. The tree-builder split works (3 cells).
2. The *first* merge (marker cell 2 into cell 1) fires — `get_reparse_type` → `MaybeCombineWithPrevCell` —
   because cell 1 (`|class="foo"`, a real syntax cell) has a (barely) valid DSR (`open_width=Some(..)`).
3. The *second* merge (marker cell 3 `bar` into the merged cell) **fails** because the merged cell carries the
   `{{!}}`-marker cell's **degenerate DSR** (`open_width=None`, `start==end`), so
   `valid_dsr_with_ws` (Condition 2) is false. The marker `<td>`s are produced by `process_special_magic_word`
   with `DataParsoid::default()` (no `tsr`), so `compute_dsr` cannot assign them a real DSR, and `merge_cells`/
   `transfer_source_between_cells` does not propagate the *source* cell's (`|class="foo"`) valid `tsr`/`dsr` onto
   the merged result — unlike PHP `reparseWithPreviousCell`, which recomputes `$prevDsr->start = $prev->tsr->start`
   from the (tsr-bearing) previous cell. Also committed a fix for the stale `prev_sibling` in `process_children`
   (now takes `out.last()` = the merged cell, matching PHP's live-DOM `previousSibling`) — `1c2958b`, net-neutral.
   **Remaining: make `merge_cells`/the marker cells inherit the source cell's `tsr`/`dsr` so the second merge's
   `valid_dsr_with_ws` passes, and recover `class="foo"` as the attribute (the `reparse_src = prev_cell_content + '|'`
   path in `reparse_with_previous_cell`, already ported, then applies).**

   ⚠️ Tried (reverted): a `merge_cells` change that copies `from.dp.tsr` + `open_width`/`close_width` onto `to`
   when `to` lacks a `tsr`. The source cell *does* carry `tsr = Some({start:6, end:7})` and `open_width=Some(1)`
   (confirmed `[MERGE] copying…`), so the merged cell ends up with `open_width=Some(1)`/`tsr=Some(…)` — yet the
   **second merge still does not fire**. So the remaining blocker is *not* just missing `open_width` on the merged
   cell; there is a second, still-unidentified condition (likely `reparse_with_previous_cell` returning `1` vs `2`,
   or the merged cell not being the `prev` for the third cell, or `puts_next_sibling_in_sol_state`). Next session:
   instrument `reparse_with_previous_cell`'s return code and the third cell's `get_reparse_type` condition
   breakdown to isolate which branch short-circuits the second merge.

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

### Token-level arg expansion — **LANDED** this turn (`40afb43`, `cdeef5b`, `3d82231`)
All of the following are now committed and verified against PHP (fixture pass count back to **821**, the
*same* failing set as baseline — net-neutral, laying the foundation for the table-cell cluster):
- `tokenize_template_arg_value` (`tokenizer_v2.rs`) — full *inline* tokenization of arg values via
  `try_inline_element` (`[[…]]`→wikilink, `'''…'''`→`mw-quote`, entities, `{{…}}`/`{{{…}}}`), newlines as
  `Nl` tokens (`newlineToken`), and every token's `dataParsoid.src` stamped from its TSR for round-trips.
- `expand_one_template` splices `templatearg` via `child_frame.expand` (no `substitute_args`), and
  `expand_templates` expands `{{{…}}}` in arg keys/values via `AttributeTransformManager::process`
  (fixes `{{#tag:pre|{{{1}}}|…}}`) and `{{!}}` → `|`/`<td>` via `process_special_magic_word`.
- `pf_tag`/`tag_extension_token` **splice** argument tokens (PHP `ParserFunctions::tag_worker` parity) and
  re-serialize content faithfully via token source (`token_to_source` + `tokens_to_string_with_nls`).
- `convert_non_html_token_to_string` treats an **empty** `attrSrc` as falsy (PHP `if ($cellAttrSrc)`), so the
  `{{!}}` magic `<td attrSrc=''>` becomes a single `|` (not `||`).
- `3d82231` — when a template expands to *all plain text* (e.g. `{{1x|!!foo}}` → `!!foo`, `{{1x|*bar}}` →
  `*bar`), the single spliced string is re-tokenized at SOL so leading table/list syntax forms; mixed
  token+string results (nested template before cell continuation) are left to the tree builder. This was
  the one token-level regression (`Spec syntactic differences …`); it's now closed.

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
- "Ensure that transclusion titles are not url-decoded"
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
- "Template interaction"
- T290526 family: "2. Using {{!}} in wikilinks", "Using {{!}} in template arguments, part 2",
  "T72875: brackets in attributes of elements in internal link texts",
  "T179544: {{anchorencode:}} output should be always usable in links"

## ⚠️ Definitively resolved this turn: the "untemplated attributes" cluster is a PHP *standalone* known-failure

The four fixtures in the "Templated table cell with untemplated attributes" cluster
(`tables.txt:1537` + "Cell combination tests" / "Regression tests" / "Integrated mode only" / "T343874")
are **`html/parsoid` (= `nativeTemplateExpansion` = *standalone*) tests whose expected output is the
**integrated-mode single-cell form** that Parsoid standalone cannot produce.**

Verified empirically (running the real `ParserTests\TestRunner` against `tables.txt` with a correct
`Template:1x ⇒ {{{1}}}`, `!` registered as a magic-word *variable* from `baseconfig/enwiki.json`
`query.variables`[0] == `!`), PHP standalone produces **two cells**, not one:

```html
<td about="#mwt1" typeof="mw:Transclusion" class="foo"
    data-parsoid='{"pi":[[{"k":"1","named":true}]],"dsr":[6,52,null,null]}'
    data-mw='{"parts":["|class=\"foo\"",{template}]}'>title="fail"</td><td about="#mwt1">bar</td>
```

and this exact two-cell output is recorded in `tables-standalone-knownFailures.json` under
"Templated table cell with untemplated attributes" (wt2html). PHP's *own* `tables.txt` `html/parsoid`
section (`<td …>title="fail"|bar</td>`, single cell) is thus the **integrated-mode ideal** — unreachable
in the standalone path that rustoid re-implements.

**Consequence for rustoid (updated after the `6cb9370` ordering fix):**
- The base "Templated table cell with untemplated attributes" fixture now **matches PHP's standalone
  two-cell output exactly** — `class="foo"` is recovered as an attribute, the transclusion metadata is
  hoisted onto the first cell — and the harness marks it SKIP (faithful divergence) rather than FAIL.
- "Cell combination tests", "Integrated mode only" and "T343874" still FAIL. The last two are
  `+integrated`-only fixtures (no `html/parsoid`/`+standalone` section), so PHP standalone cannot reach
  their expectations either — see the note at the top of this file.
- These fixtures still cannot reach the single-cell `html/parsoid` ideal (PHP standalone can't either —
  it's a recorded known-failure); matching PHP's standalone output is the correct, faithful target.

### Re-derived this turn (why no `processSpecialMagicWord` fired in standalone probes)
`{{!}}` is a magic-word **variable** (ID `!`, canonical `!`; `baseconfig/enwiki.json` `variables[0]`).
`resolveTemplateTarget("!")` returns `magicWordType === '!'`, so `processSpecialMagicWord` *does* run and
returns `<td attr_src=''` `AT_SRC_START>` — producing the sibling-cell split that ends at the two-cell
known-failure. Which of its two branches is taken depends on `inTemplate`: it is `true` for an
**argument value** (PHP expands argument keys/values under a hard-coded `inTemplate => true`) and the
*caller's* flag for the **template body**. (Earlier probes that "got `|`" were using `MockSiteConfig`
including no `!` variable, so `{{!}}` fell through to a redlink/`convertToString` path.) The
`pipe = "|" / "{{!}}"` tokenizer rule handles `{{!}}` only in table *position* (as `attrSepSrc`), not
inside argument values (tokenized with `table=false`).

### Root cause of the rustoid divergence — **FOUND & FIXED** (pipeline ordering)
The divergence was **not** the tree builder (as the prior note guessed). PHP's `dom:post-builder` dump
confirmed its tree builder *also* splits `{{!}}` into **three** `<td>` cells (source `class="foo"` +
meta, `title="fail"`, `bar`), exactly like rustoid. The difference is **`ComputeDSR` runs too early in
rustoid**: PHP's `NESTED_PIPELINE_DOM_TRANSFORMS` order is `… pwrap … migrate-metas … migrate-nls …
dsr … tplwrap …`, but rustoid's `build_ast`/`wikitext_to_ast` ran `compute_dsr` *immediately after tree
building* (before `p-wrap` and `migrate-metas`). Running `dsr` against the un-canonicalized DOM stamped
the source cell with `dsr=(6,53,1,0)` (its `end` leaked out to the table end) instead of PHP's
`(6,18,1,0)` (bounded at the transclusion START marker's `tsr->start == 18`, i.e. exactly
`|class="foo"`).

**FIXED** (`6cb9370`): moved `compute_dsr` into `post_pwrap_transforms`, between `migrate-nls` and
`encapsulate_transclusions` (tplwrap), matching PHP's order. The base "Templated table cell with
untemplated attributes" fixture now produces PHP's standalone 2-cell output exactly — `class="foo"` is
recovered as an attribute via `reparseWithPreviousCell` (`prevCellContent="class=\"foo\""`) — and the
harness recognizes it as a faithful divergence (SKIP) rather than a bug.

Remaining in this cluster: "Cell combination tests", "Integrated mode only" (its `&nbsp;{{!}}` second
row), and "T343874" still FAIL. **Updated** (`94782e7`): the `&nbsp;` row now merges and recovers
`class="foo"` — the `mw:Entity` child was *not* the blocker. (The real blocker was that `{{…}}` was
rejected as a table attribute name in cell-argument position, so the `k=v|` reparse of
`class="foo"{{1x|1=&nbsp;{{!}}bar}}|` recovered nothing.) What remains for that row is the missing
`about` on the merged cell, and both `Integrated mode only` and `T343874` are `+integrated`-only
fixtures whose expectations PHP standalone cannot reach — see the note at the top of this file.

Also landed this turn (faithful): `compute_dsr` now gates the `cs = s` fallback on `i == 0` (leftmost
child), matching PHP's `elseif ($s && $child->previousSibling === null)`; unit test
`test_tsr_less_later_sibling_does_not_inherit_s` locks it in.

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
