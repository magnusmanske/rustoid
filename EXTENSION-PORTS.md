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

### `section`

LST's `<section begin="X"/>` / `<section end="X"/>` markers select a labelled
region; Parsoid emits a void `<link>` carrying the extension metadata and no
content. rustoid fell through to its generic `<extension>` placeholder. The
`about` id is spent in `number_extension_token` (as for every extension except
`<nowiki>`), and `section_items` builds the `<link>`. Empty-valued attributes are
kept, because the served `data-mw` keeps them
(`{"begin":"Schools","of":"","thought":""}`). Source: measured served bytes plus
`LabeledSectionTransclusion`.

## `imagemap` — next, but not bounded

4 corpus pages. Parsoid-native: the handler is
`mediawiki-extensions-ImageMap/includes/ParsoidImageMap.php`. The **algorithm is
small** (read below); the **integration is not**, which is the important finding.

### Algorithm (`ParsoidImageMap::sourceToDom`)

1. Line-split the tag body. Skip blank lines and lines starting with `#`.
2. First real line is the image: `File:X.png | 100px | alt=…`. Everything after
   the first `|` (re-prefixed with `|`) is passed to `renderMedia`, rendered as a
   **block** (`|none`), giving a `<figure>` (`thumb`).
3. `scale = (thumbWidth + thumbHeight) / (imageWidth + imageHeight)`, taken from
   the rendered `<img>`'s `width`/`height` and `data-file-width`/`data-file-height`.
4. For each further line:
   - `desc <type>` sets the caption position (`top-right|bottom-right|bottom-left|top-left|none`).
   - Otherwise the link is everything from the first `[`; the remainder is the
     shape spec: `default` (no coords), `rect` (4), `circle` (3), `poly` (even).
   - Coordinates are scaled by `scale` and rounded.
   - One `<area>` per line with `href`, `shape` (unless `default`),
     `coords`, `alt` (= link text, unless `default`), `title`; external links also
     get `class="plainlinks"` + `rel`/`target`.
5. If any areas: build `<map>`, name it
   `ImageMap_` + first **16 hex of `md5(innerHTML of <map>)`**, set
   `usemap="#<name>"` on the `<img>`, and insert the `<map>` after the image's
   wrapper.
6. Add `noresize` to the `<figure>`; unless it is a real thumbnail add
   `mw-ext-imagemap-desc-<type>`.
7. Replace the media `<a>` wrapper with an `<a>` (default-shape link) or a
   `<span>`, containing the `<img>`; carry the caption text to its `title`.

Served shape (Earth), abbreviated:

```html
<figure class="mw-default-size noresize" typeof="mw:File mw:Extension/imagemap"
        about="#mwt1443" data-mw='{"name":"imagemap","attrs":{},"body":{"extsrc":"…"}}'>
  <span><img resource="./File:…" … usemap="#ImageMap_818ebd24fd44a6df"/></span>
  <map name="ImageMap_818ebd24fd44a6df"><area href="./Sun" shape="rect" coords="0,0,90,35" alt="The Sun" title="The Sun"/>…</map>
</figure>
```

### Why it is larger than it looks

- **Media dimensions aren't available where Parsoid has them.** Parsoid's
  `renderMedia` is synchronous and returns the thumbnail with its size, so it can
  compute `scale` inline. rustoid renders media to a `<span typeof="mw:File">`
  *placeholder* and resolves sizes in the later async `add_media_info` pass. So
  either (a) the tag is handled in a **post-`add_media_info` DOM pass** once the
  `<img>` exists, or (b) it is handled at expansion time and the size is read from
  `DataSource::get_file_info` instead of the rendered `<img>`. The common case has
  no size option, so `scale == 1`; option (b) with `get_file_info` covers the rest.
- **The map name needs `md5`.** rustoid does not compute md5 today (thumbnail URLs
  come from the wiki's API). This adds a small pure-Rust dependency, or a ~100-line
  MD5. Justified, but new.
- **Each line's link is a nested render.** `[[Sun|The Sun]]` must go through the
  wikilink/renderFile pipeline to get its `href` (and to know it is external), the
  same nested-pipeline machinery `gallery::render_gallery_media` already uses.
- **The figure is post-processed.** Insert the `<map>`, set `usemap`, replace the
  `<a>` with the default `<a>`/`<span>`, add classes, and put
  `mw:Extension/imagemap` on the figure's `typeof` alongside `mw:File`.

### Plan

1. Add `md5` (pure Rust) for the map name.
2. Write `pipeline/imagemap.rs`: a tested `parse_body(&str) -> ImagemapSpec`
   (`image`, `options`, `desc`, `areas`), ported field-for-field from the PHP above.
3. Add `Parser::expand_imagemap`, modelled on `expand_gallery` (same call site, same
   `mw:DOMFragment` placeholder + `about` handling), rendering the image through the
   existing media path and each link through the existing wikilink path.
4. Take `scale` from `DataSource::get_file_info` at expansion (option b), so it needs
   no second pass.
5. Verify against the Earth oracle; the `<map>` children are plain `<area>`s, so a
   token-level compare (`tagdiff`) localises any mismatch quickly.

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
