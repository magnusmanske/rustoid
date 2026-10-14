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

## `math` (5 pages)

**Not winnable by a faithful port.** The serving path is *Mathoid*, Wikimedia's
MathJax-based service, not a local renderer. Probed through the transform
endpoint, one `<math>` becomes

```html
<span class="mwe-math-element mwe-math-element-inline" typeof="mw:Extension/math"
      about="#mwt1" data-mw='{"name":"math","attrs":{},"body":{"extsrc":"…"}}' id="mwAw">
  <span class="mwe-math-mathml-a11y" style="display: none;">
    <math xmlns="http://www.w3.org/1998/Math/MathML" class="mathjax_ignore" alttext="…">
      <semantics>
        <mrow class="MJX-TeXAtom-ORD">
          <mstyle displaystyle="true" scriptlevel="0"> … </mstyle>
```

Two independent blockers:

1. The MathML is **MathJax's** (the `MJX-TeXAtom-ORD` wrapper around every group,
   `mstyle displaystyle="true" scriptlevel="0"` at the root, the
   `<annotation encoding="application/x-tex">`). Reproducing it means porting
   MathJax's TeX→MathML conversion, not the doc's earlier guess of "KaTeX".
2. The visible output is `<img src="//wikimedia.org/api/rest_v1/media/math/render/
   svg/<sha1>" … style="vertical-align: -0.838ex; width:22.769ex; height:2.843ex;"/>`,
   a hashed *Mathoid* URL. The `sha1` is not a hash of the TeX (checked), and the
   `width`/`height`/`vertical-align` values are MathJax's own layout metrics.

Both fall outside "parse, don't validate": there is no local function from the
TeX source to those bytes. Treat `math` like `mapframe`, not as a large-but-finite
port.

## `syntaxhighlight` (4 pages, 171 tags)

Deterministic and local: SyntaxHighlight_GeSHi shells out to **Pygments**, and
Parsoid serialises its token spans directly (`<div class="mw-highlight
mw-highlight-lang-…">` with `mw-highlight-*` spans). Unlike `math` there is no
service and no metrics — only a Pygments subset (`wikitext`, `python`, `python2`,
`python3`, `text`, `css`, `js`, `lua`) to port. This is the one large-but-winnable
highlighter; worth doing once a page is otherwise within reach of 100 %.

## The literal-`<extension>` leak bucket

The scoreboard's `extension` outcome (25 of 44 non-matching pages) is *not* a
homogeneous bucket: `mw:Extension/ref` (Cite) and
`mw:Extension/templatestyles` are on almost every page, so the classifier — which
fires on the first `mw:Extension` in the differing region — labels any diff that
merely sits next to one. Splitting the bucket by the actual cause, the pages
carrying an *unrendered* `<extension>` tag (rustoid fell through to its generic
placeholder) are only:

| Leaked tag | Pages | Where |
| --- | --- | --- |
| `syntaxhighlight` | 4 | `Template:Infobox` (91), `Python` (62), `Template:Weather box` (15), `Brat` |
| `math` | 5 | `Sundial` (66), `Tropical cyclone` (7), `Hydrogen` (3), `India` (2), `Earth` (1) |
| `mapframe`/`maplink` | 4 | `2024 Summer Olympics`, `Titanic`, `Chernobyl disaster`, `Berlin` |
| `nowiki` | 2 | `Grand Theft Auto V` (15), `Brat` (6) — a *parser* bug, not a missing extension |
| `timeline` | 1 | `The Beatles` |
| `categorytree` | 1 | `Isaac Newton` |

The `nowiki` leak was the `#switch`-argument bug fixed below (a `</nowiki>` scan
over-ran its `>`); the other leaks are the extension ports named here. Everything
else tagged `extension` first-diverges on Cite numbering or a templatestyles link
and its true cause is elsewhere (often a Lua/magic-word value).

## Not winnable by faithful implementation

- Media derivative `utm_campaign`, and `mapframe`: data/map dependent. The media
  campaign is masked by the comparator.
- `math`: Mathoid/MathJax output and layout metrics (see above).
