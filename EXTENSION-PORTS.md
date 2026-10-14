# Parser extensions — status and implementation plans

`enwiki-parser-extensions.md` is the *inventory* (which tags the wiki runs).
This file is the *work log and plan* for porting the ones Parsoid output needs,
derived by reading the PHP sources, not by guessing shapes.

The corpus numbers referenced here are from an `--offline` run of the built-in
48-page corpus.

## Done

| Extension | Pages | Notes |
|---|---|---|
| Cite (`ref`/`references`) | 44 | `rustoid-extensions` |
| `section` (LabeledSectionTransclusion) | 4 | renders a void `<link typeof="mw:Extension/section" …/>` |
| `imagemap` (ImageMap) | 4 | `pipeline/imagemap.rs` + `Parser::build_imagemap` |
| `phonos` (Phonos) | 10 | `pipeline/phonos.rs` + the minimal OOUI serializer |
| `templatedata` (TemplateData) | 2 | `pipeline/templatedata.rs` (parameter-documentation table) |
| `inputbox` (InputBox) | 1 | `pipeline/inputbox.rs` (the `search`/`fulltext` forms) |

### `section`

LST's `<section begin="X"/>` / `<section end="X"/>` markers select a labelled
region; Parsoid emits a void `<link>` carrying the extension metadata and no
content. rustoid fell through to its generic `<extension>` placeholder. The
`about` id is spent in `number_extension_token` (as for every extension except
`<nowiki>`), and `section_items` builds the `<link>`. Empty-valued attributes are
kept, because the served `data-mw` keeps them
(`{"begin":"Schools","of":"","thought":""}`). Source: measured served bytes plus
`LabeledSectionTransclusion`.

### `imagemap`

4 corpus pages. Parsoid-native handler `ParsoidImageMap::sourceToDom`. Ported as
`pipeline/imagemap.rs` (the line grammar, the `desc` placement, the `<area>`
construction, the `<map>` name) plus `Parser::build_imagemap` /
`expand_imagemap` (rendering), modelled on `expand_gallery`. Two details the
served bytes force: the media is rendered as a block (`|none`) and the
`mw-halign-none` that produces is stripped unless the source asked for `none`
(`renderMedia`'s `forceBlock` check); and the `<map>` name is `ImageMap_` + 16 hex
of `md5(serialized <area> children)`, computed before the id pass runs. All four
map names now match Parsoid's byte for byte.

Porting it surfaced two argument bugs *upstream* of the handler, both now fixed
(see `ONLINE-PARITY.md`): `#tag` must take its content positionally, and the
lowering of `frame:extensionTag` must keep the (possibly empty) content slot.

### `phonos`

10 corpus pages. A legacy MediaWiki hook, so the wiki serves OOUI's HTML: a
`PhonosButton` (frameless, `volumeUp`, no visible label) inside a
`<span class="ext-phonos">`, then an attribution `<sup>`. Rendered by
`pipeline/phonos.rs`, which drives the minimal OOUI serializer in
`pipeline/ooui.rs` — the `oo-ui-*` class vocabulary and its order, the
`data-ooui` JSON (a PHP-`json_encode`-compatible encoder, because the key order
and the `/`-escaping are visible), and the `ooui-php-N` counter (one per page).

`data.file` is the *resolved* file title — a redirect and the wiki's first-letter
capitalization both applied, which the API reports only through the description
URL — and the button's `href` is TimedMediaHandler's mp3 transcode URL, already
in the file facts' `videoinfo` derivatives. The attribution link uses the wiki's
`wfUrlencode`d `/wiki/File:…` URL. The tag body is the visible label, parsed in
full. Two supporting fixes: a ref/note body now runs the extension passes (so a
`{{#tag:phonos|…}}` inside a note renders), and `phonos` is no longer treated as
native content (Parsoid discards its output's `data-parsoid` inside a
transclusion range, so its button takes no ids).

Every phonos region matches the oracle byte for byte, bar the global id counter;
`Israel` still renders one `Audio` twice, a pre-existing duplication unrelated to
the widget.

### `templatedata`

2 corpus pages (a third, `Brat (album)`, leaks the tag from a `/doc` page Parsoid
never transcludes — a separate `noinclude` difference). A legacy hook: the tag's
JSON body is validated and normalized, then rendered as a parameter
documentation table. `pipeline/templatedata.rs` is a faithful port of
`TemplateDataHtmlFormatter::getHtml` plus the parts of
`TemplateDataBlob`/`TemplateDataNormalizer` the formatter reads (interface texts
resolved to the content language, per-param defaults). The `mw:edittemplatedata`
marker names the frame the tag sits in (`$frame->getTitle()` — the template's
`/doc` subpage), which a template body's token carries. Like `<phonos>`,
`<templatedata>` is not native content, so its table takes no ids.

### `inputbox`

The `Help:Introduction` search box. A legacy hook: the tag body is a
`key=value` option list (`InputBox::extractOptions`) and `InputBox::render`
builds one of several forms. `pipeline/inputbox.rs` ports the `search` and
`fulltext` forms (`InputBox::getSearchForm` and its `buildTextBox`/
`buildCheckboxInput`/`buildSubmitInput` helpers), built as DOM nodes so the usual
`typeof`/`about`/`data-mw` encapsulation applies. The checkbox `id` is
`wfRandom()`, which the comparator masks (`0.<8+ digits>`); rustoid emits a
deterministic one. The edit forms (`create`/`comment`/`move`) are left to the
generic `<extension>` fallback until a page needs them. Not native content, so
`is_native_ext` excludes it.

## `mapframe` (4 pages)

Kartographer needs map tiles/data and is likely **unwinnable offline**; treat
like the other nondeterministic cases.

## `math` (5 pages) and `syntaxhighlight` (4 pages, 171 tags)

Parsoid-native and deterministic, but each needs a large third-party algorithm
ported faithfully:

- `math` → KaTeX's HTML output (`<span class="mwe-math-element">…` with
  `mwe-math-*` spans). The Math extension's Parsoid handler calls a
  KaTeX-compatible renderer.
- `syntaxhighlight` → SyntaxHighlight_GeSHi's Pygments tokenization
  (`<code class="mw-highlight mw-highlight-lang-…">` with `mw-highlight-*` spans).
  Most corpus occurrences are `<syntaxhighlight lang="wikitext" inline
  style="display:none">` in template docs.

These are separate, large efforts (a KaTeX subset / a Pygments subset). Worth
doing only once a page is otherwise within reach of 100 %, because neither can be
approximated without failing the byte comparison.

## Not winnable by faithful implementation

- Media derivative `utm_campaign`, and `mapframe`: data/map dependent. The media
  campaign is masked by the comparator.
