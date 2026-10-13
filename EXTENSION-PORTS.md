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

## OOUI foundation — gates `phonos` (10 pages), `templatedata` (3), `mapframe` (4)

`phonos` and `templatedata` are **legacy parser hooks**, not Parsoid-native
extensions, so what the oracle serves is **OOUI's** serialization. Measured:

```html
<span class="ext-phonos" typeof="mw:Extension/phonos" about="#mwt70"
      data-mw='{"name":"phonos","attrs":{"file":"…"},"body":{"extsrc":""}}'>
  <span data-nosnippet="" id="ooui-php-1" class="noexcerpt ext-phonos-PhonosButton
        … oo-ui-widget oo-ui-widget-enabled oo-ui-buttonElement …"
        data-ooui='{"_":"mw.Phonos.PhonosButton","href":"//upload.wikimedia.org/…mp3",
                    "rel":["nofollow"],"framed":false,"icon":"volumeUp",
                    "data":{"ipa":"","text":"","lang":"en","wikibase":"","file":"…"},…}'>
    <a role="button" tabindex="0" href="…" rel="nofollow" aria-label="Play audio"
       title="Play audio" class="oo-ui-buttonElement-button">
      <span class="oo-ui-iconElement-icon oo-ui-icon-volumeUp"></span>
      <span class="oo-ui-labelElement-label"></span>
      <span class="oo-ui-indicatorElement-indicator oo-ui-indicatorElement-noIndicator"></span>
    </a>
  </span>
  <sup class="ext-phonos-attribution noexcerpt navigation-not-searchable">…</sup>
</span>
```

Matching this byte-for-byte means reproducing OOUI's HTML: the `oo-ui-*` class
lists, the `data-ooui` JSON (`_`, `href`, `rel`, `framed`, `icon`, `data`,
`classes`), and the `ooui-php-N` element counter. So it is **not** a per-extension
task; it is one shared serializer plus two thin config builders
(`PhonosButton`, and TemplateData's widget). Recommendation: port a *minimal* OOUI
HTML serializer for exactly the widgets these two use (button element, icon,
label, indicator, field layouts) rather than attempting OOUI in general, and drive
it from two builders. That single piece unlocks 13 pages.

`phonos` additionally needs TimedMediaHandler's mp3 transcode URL
(`getSources(...).transcodekey == 'mp3'`), which is available from the file facts
the media pass already fetches.

`mapframe` (Kartographer) needs map tiles/data and is likely **unwinnable
offline**; treat like the other nondeterministic cases.

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

- `<inputbox>` (Help:Introduction): the input's id is `mw-inputbox-ns12-0.<mt_rand()>`.
  The new comparator masks it, but the page has other real differences, so it is
  not yet `Equivalent`.
- Media derivative `utm_campaign`, and `mapframe`: data/map dependent. The media
  campaign is masked by the comparator.
