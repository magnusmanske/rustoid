//! `<gallery>` extension — faithful port of PHP Parsoid's
//! `Ext/Gallery` (traditional mode) for the wt2html direction.
//!
//! Implements the subset of `Gallery::sourceToDom`/`TraditionalMode::render` that
//! the parser-test fixtures exercise: traditional mode with `widths`/`heights`/
//! `perrow`/`caption`/`mode`/`showfilename`/`class`/`style` options. Other modes
//! (nolines, slideshow, packed, packed-overlay, packed-hover) are deferred.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::title::{Title, TitleParser};
use crate::traits::SiteConfig;
use crate::wikitext::tokens_v2::{Item, ParsoidToken};

/// The default gallery options (mirrors `SiteConfig::galleryOptions()`).
const DEFAULT_IMAGE_WIDTH: u32 = 120;
const DEFAULT_IMAGE_HEIGHT: u32 = 120;

/// Per-mode box padding (mirrors the `padding` objects on `TraditionalMode` and
/// its subclasses). `border` is the per-image border used only in the `perrow`
/// max-width computation.
struct Padding {
    thumb: u32,
    box_padding: u32,
    border: u32,
}

fn padding_for_mode(mode: &str) -> Padding {
    match mode {
        // `NoLinesMode`: thumb 0, box 5, border 4.
        "nolines" => Padding {
            thumb: 0,
            box_padding: 5,
            border: 4,
        },
        // `PackedMode`/`Packed-overlay`/`Packed-hover`: thumb 0, box 2, border 8.
        "packed" | "packed-overlay" | "packed-hover" => Padding {
            thumb: 0,
            box_padding: 2,
            border: 8,
        },
        // `TraditionalMode` and `SlideshowMode`: thumb 30, box 5, border 8.
        _ => Padding {
            thumb: 30,
            box_padding: 5,
            border: 8,
        },
    }
}

/// The packed/overlay/hover scale factor (mirrors `PackedMode::__construct`).
const PACKED_SCALE: f64 = 1.5;

/// The gallery's default thumbnail dimensions as a media `width`/`height` option
/// string (mirrors `TraditionalMode::dimensions` → `"{w}x{h}px"`, and
/// `PackedMode::dimensions` for the packed/overlay/hover modes, which request a
/// large pre-scaling thumbnail).
fn gallery_dimensions(opts: &GalleryOpts) -> String {
    if matches!(
        opts.mode.as_str(),
        "packed" | "packed-overlay" | "packed-hover"
    ) {
        let (w, h) = packed_dimensions(opts.image_height);
        format!("{w}x{h}px")
    } else {
        format!("{}x{}px", opts.image_width, opts.image_height)
    }
}

/// The requested (pre-scaling) thumbnail dimensions for packed/overlay/hover
/// modes, mirroring `PackedMode::dimensions`: a large width so the height is
/// not the constraining factor, both scaled by `PACKED_SCALE`. Returns
/// `(width, height)`; the caller stores these as `data-width`/`data-height` on
/// the broken-media span (so `AddMediaInfo` requests the large thumbnail).
fn packed_dimensions(image_height: u32) -> (u32, u32) {
    let height = ((image_height as f64) * PACKED_SCALE).floor() as u32;
    // The legacy parser does this so the width is not the constraining factor.
    let width = (((image_height * 10 + 100) as f64) * PACKED_SCALE).floor() as u32;
    (width, height)
}

/// Parsed `<gallery>` options.
#[derive(Debug)]
struct GalleryOpts {
    image_width: u32,
    image_height: u32,
    images_per_row: u32,
    mode: String,
    showfilename: bool,
    showthumbnails: bool,
    caption: String,
    /// Additional sanitized attributes (`class`, `style`, …) applied to the
    /// `<ul>`.
    attrs: Vec<(String, String)>,
}

impl Default for GalleryOpts {
    fn default() -> Self {
        Self {
            image_width: DEFAULT_IMAGE_WIDTH,
            image_height: DEFAULT_IMAGE_HEIGHT,
            images_per_row: 0,
            mode: "traditional".to_string(),
            showfilename: false,
            showthumbnails: false,
            caption: String::new(),
            attrs: Vec::new(),
        }
    }
}

/// A single parsed gallery line's file title and caption are rendered inline in
/// `render_line` (no intermediate struct is needed).
///
/// Build the `<ul class="gallery …">` DOM fragment for a `<gallery>` extension.
/// Each line becomes a `<li class="gallerybox">` carrying a `.thumb` div with a
/// broken-media `<span typeof="mw:File">` (resolved later by `AddMediaInfo`) and
/// a `.gallerytext` div with the caption.
pub fn build(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
) -> Node {
    build_with_sync(token, config)
}

/// Build the `<ul …>` fragment. Two renderers are supplied:
/// - `render_caption` renders the `caption=` attribute (inline wikitext).
/// - `render_media(title, opts)` renders a single line's media as a block
///   `<figure>` (with the caption in a `<figcaption>` and the `title`/`alt`
///   already resolved), mirroring PHP's `ParsoidExtensionAPI::renderMedia`
///   (`forceBlock=true`); it returns `None` when the line is not a valid file.
pub async fn build_with<F, Fut, G, GFut>(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
    mut render_caption: F,
    mut render_media: G,
) -> Node
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = Vec<Node>>,
    G: FnMut(&str, &str) -> GFut,
    GFut: std::future::Future<Output = Option<Node>>,
{
    let opts = parse_opts(token, config);
    let source = token
        .attribs
        .iter()
        .find(|kv| kv.key.as_str() == Some("source"))
        .and_then(|kv| kv.value.as_str())
        .unwrap_or("")
        .to_string();
    let body = crate::pipeline::extension_handler::extract_ext_body(token, &source);

    let mut ul = Node::element(ElementKind::UnorderedList);
    let mut class = format!("gallery mw-gallery-{}", opts.mode);
    // Optional `class` user attribute appended (mirrors `appendAttr`).
    if let Some(user_class) = opts
        .attrs
        .iter()
        .find(|(k, _)| k == "class")
        .map(|(_, v)| v.as_str())
    {
        class.push(' ');
        class.push_str(user_class);
    }
    ul.set_attr("class", class);
    ul.set_attr("typeof", "mw:Extension/gallery");

    // perrow → max-width on the <ul> (mirrors `TraditionalMode::perRow`);
    // slideshow mode ignores perrow entirely. This is applied BEFORE the user
    // attributes, matching `TraditionalMode::ul` (perRow first, then attrs).
    if opts.images_per_row > 0 && opts.mode != "slideshow" {
        let padding = padding_for_mode(&opts.mode);
        let total = opts.image_width + padding.thumb + padding.box_padding + padding.border;
        let total = total * opts.images_per_row;
        append_attr(&mut ul, "style", &format!("max-width: {total}px;"));
    }

    // Remaining sanitized attrs (style, data-test, …) appended after the
    // defaults (mirrors `TraditionalMode::ul`, which loops `$opts->attrs` and
    // `appendAttr`s each onto the `<ul>`).
    for (k, v) in &opts.attrs {
        if k == "class" {
            continue;
        }
        append_attr(&mut ul, k, v);
    }

    // slideshow `showthumbnails` → `data-showthumbnails="1"`/`""` (mirrors
    // `SlideshowMode::setAdditionalOptions`).
    if opts.mode == "slideshow" {
        ul.set_attr(
            "data-showthumbnails",
            if opts.showthumbnails { "1" } else { "" },
        );
    }

    // data-mw names the extension (stripped in harness comparison, but set for
    // round-trip fidelity).
    ul.data_mw = Some(r#"{"name":"gallery","attrs":{},"body":{}}"#.to_string());

    // A non-empty `caption=` renders a leading `<li class="gallerycaption">`
    // (mirrors `TraditionalMode::caption`).
    if !opts.caption.is_empty() {
        let mut li = Node::element(ElementKind::ListItem);
        li.set_attr("class", "gallerycaption");
        for node in render_caption(&opts.caption).await {
            li.push_child(node);
        }
        ul.push_child(li);
    }

    // Parse and render each line.
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(li) = render_line_with(&opts, line, config, &mut render_media).await {
            ul.push_child(li);
        }
    }

    ul
}

/// Synchronous [`build_with`] entry point (used by the `wikitext_to_ast` path
/// and tests): render each caption through [`caption_to_nodes`] and each media
/// line through a no-expansion `renderFile` (see [`render_media_sync`]), blocking
/// on the immediately-ready future.
pub fn build_with_sync(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
) -> Node {
    use std::task::{Context, Poll, Waker};

    let fut = build_with(
        token,
        config,
        |caption: &str| std::future::ready(caption_to_nodes(caption, config)),
        |title: &str, opts: &str| std::future::ready(render_media_sync(title, opts, config)),
    );
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(node) => node,
        // `caption_to_nodes`/`render_media_sync` return ready futures, so this
        // branch is unreachable in practice.
        Poll::Pending => unreachable!("gallery media future is always ready"),
    }
}

/// Render a single gallery line's media as a block `<figure>` without template
/// expansion (used by the synchronous `wikitext_to_ast` path): tokenize
/// `[[title|opts|none]]` and run it through `renderFile` with media formats
/// suppressed. Mirrors `render_gallery_media` minus template expansion and the
/// `AddMediaInfo` pass.
fn render_media_sync(title_str: &str, opts_str: &str, config: &dyn SiteConfig) -> Option<Node> {
    use crate::pipeline::parser::render_inline_fragment;
    use crate::pipeline::wiki_link_render::{
        WikiLinkContext, get_wiki_link_target_info, render_wiki_link_dispatched,
    };
    use crate::wikitext::token_utils::key_value_to_string;

    let wikitext = format!("[[{title_str}|{opts_str}|none]]");
    let options = crate::wikitext::tokenizer_v2::TokenizerOptions::default();
    let mut tokenizer = crate::wikitext::tokenizer_v2::PegTokenizer::new(&wikitext, &options);
    let tokens = tokenizer
        .tokenize()
        .unwrap_or_default()
        .into_iter()
        .map(|c| match c {
            crate::wikitext::tokens_v2::Either::Left(s) => Item::Str(s),
            crate::wikitext::tokens_v2::Either::Right(t) => Item::Tok(t),
        })
        .collect::<Vec<_>>();

    let mut link_ctx = WikiLinkContext::new(config);
    link_ctx.set_suppress_media_formats();
    let mut fragments = std::collections::HashMap::new();
    let mut next_id = 0usize;
    let tokens: Vec<Item> = tokens
        .into_iter()
        .flat_map(|item| {
            let Item::Tok(ParsoidToken::SelfclosingTag(stt)) = &item else {
                return vec![item];
            };
            if stt.name != "wikilink" {
                return vec![item];
            }
            let href = stt
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("href"))
                .map(|kv| key_value_to_string(&kv.value))
                .unwrap_or_default();
            let href_src = href.clone();
            let target = match get_wiki_link_target_info(&link_ctx, &href, &href_src) {
                Ok(t) => t,
                Err(_) => return vec![Item::Str(format!("[[{href}]]"))],
            };
            render_wiki_link_dispatched(
                &mut link_ctx,
                &ParsoidToken::SelfclosingTag(stt.clone()),
                &target,
                false,
                &mut fragments,
                &mut next_id,
                &mut |items| {
                    let mut f = std::collections::HashMap::new();
                    let mut id = 0usize;
                    render_inline_fragment(config, items, &mut f, &mut id)
                },
            )
        })
        .collect();
    let frag = render_inline_fragment(config, tokens, &mut fragments, &mut next_id);
    frag.children.into_iter().next()
}

/// [`render_line`] with a caller-supplied media renderer (see [`build_with`]).
/// Mirrors `Gallery::pLine` + `TraditionalMode::line`.
async fn render_line_with<G, GFut>(
    opts: &GalleryOpts,
    line: &str,
    config: &dyn SiteConfig,
    render_media: &mut G,
) -> Option<Node>
where
    G: FnMut(&str, &str) -> GFut,
    GFut: std::future::Future<Output = Option<Node>>,
{
    let line = line.trim();
    // Split on the first `|` (title | caption+options).
    let (title_str, mut rest_str) = match line.split_once('|') {
        Some((t, r)) => (t.trim(), r.to_string()),
        None => (line, String::new()),
    };

    // A common editor mistake is closing a gallery line with `]]` (from a
    // converted `[[File:…]]` wikilink). Strip a trailing `]]` unless the option
    // string contains `[[` (a pending wikilink in a caption). Mirrors
    // `Gallery::pLine`.
    if !rest_str.contains("[[") {
        rest_str = rest_str.strip_suffix("]]").unwrap_or(&rest_str).to_string();
    }

    // Title resolution: decode entities (`&#45;` → `-`, `&amp;` → `&`) and
    // percent-escapes (`%26` → `&`), mirroring `Gallery::pLine`'s entity-decoding
    // plus the tokenizer's URL-decoding inside `renderMedia`/`renderFile`.
    let file_ns = config.canonical_namespace_id("File").unwrap_or(6);
    let decoded_entities =
        crate::html::wts_utils::decode_wt_entities_all(&title_str.replace("_", " "));
    let decoded = crate::util::decode_uri_component(&decoded_entities);
    // A title with illegal characters (e.g. `[[x`) is rejected, mirroring
    // `makeTitle` returning null in `Gallery::pLine` (the line is then dropped).
    if crate::title::has_invalid_chars(&decoded) {
        return None;
    }
    let mut title = TitleParser::parse(&decoded, config);
    let no_prefix = title.namespace_id != file_ns;
    if no_prefix {
        // Re-parse with an explicit `File:` prefix so first-letter capitalization
        // (ucfirst) is applied to the title text (mirrors `renderMedia`'s
        // `makeTitle( $decodedTitleStr, $fileNs )`).
        title = TitleParser::parse(&format!("File:{decoded}"), config);
    }
    if title.namespace_id != file_ns {
        return None;
    }
    // The wikilink target: a namespace-less title gets an explicit `File:` prefix
    // (mirrors PHP's `$titleStr = $noPrefix ? $title->getPrefixedDBKey() : $oTitleStr`).
    let link_title = if no_prefix {
        title.get_prefixed_text()
    } else {
        title_str.to_string()
    };

    // Append the gallery's default dimensions so `renderFile` stamps
    // `data-width`/`data-height` on the broken span (mirrors PHP's
    // `$imageOpts[] = "|{$mode->dimensions($opts)}"`).
    let opts_with_dims = format!("{rest_str}|{}", gallery_dimensions(opts));

    // Render the line's media as a block figure via the full `renderFile`
    // pipeline (mirrors `renderMedia(forceBlock=true, suppressMediaFormats=true)`);
    // `None` means the line is not a valid file and is dropped.
    let mut figure = render_media(&link_title, &opts_with_dims).await?;

    // `hasError` (a missing/bad file) is decided from the figure's `mw:Error`
    // RDFa type after `AddMediaInfo` runs (mirrors `ParsedLine::__construct`).
    let has_error = figure
        .get_attr("typeof")
        .is_some_and(|ty| ty.split_whitespace().any(|tok| tok == "mw:Error"));

    // Detach the `<figcaption>` (becomes `.gallerytext`) from the figure.
    let caption_nodes = take_figcaption(&mut figure);

    // Packed/overlay/hover modes re-scale the resolved `<img>` (mirrors
    // `PackedMode::scaleMedia`, called by `TraditionalMode::line` *after*
    // `renderMedia`/`AddMediaInfo`). `width` becomes the unrounded
    // `renderedWidth / scale` for box sizing; other modes use `imageWidth`.
    let scale_width = scale_media(&mut figure, opts);

    // `thumbWidth` (PackedMode: at least 60px so the caption is wide enough;
    // Traditional modes: width + padding.thumb) and `boxWidth = thumbWidth +
    // padding.box` drive the `.thumb`/`.gallerybox` styles.
    let padding = padding_for_mode(&opts.mode);
    let thumb_width = gallery_thumb_width(scale_width, &padding);
    let thumb_height = opts.image_height + padding.thumb;
    let box_width = thumb_width + padding.box_padding as f64;

    // `<li class="gallerybox" style="width: <boxWidth>px;">`
    let mut li = Node::element(ElementKind::ListItem);
    li.set_attr("class", "gallerybox");
    li.set_attr(
        "style",
        format!("width: {}px;", fmt_gallery_width(box_width)),
    );
    li.data_mw = Some("{}".to_string());

    // `<div class="thumb" style="…">` — the width is omitted for an error thumb
    // (mirrors `TraditionalMode::thumbStyle`), and the height only appears in
    // `traditional` mode or when there is an error.
    let mut thumb = Node::element(ElementKind::Div);
    thumb.set_attr("class", "thumb");
    let mut style = String::new();
    if !has_error {
        style.push_str(&format!("width: {}px; ", fmt_gallery_width(thumb_width)));
    }
    if has_error || opts.mode == "traditional" {
        style.push_str(&format!("height: {}px;", thumb_height));
    }
    thumb.set_attr("style", style.trim_end());

    // Wrap the figure (a `<figure typeof="mw:File">`) as an inline `<span>` and
    // migrate its children (the `<a>`→`<img>`) into it, mirroring
    // `TraditionalMode::line`. The `mw-halign-*` class from the forced `|none` is
    // dropped.
    thumb.push_child(figure_to_span(&mut figure));
    li.push_child(thumb);

    // `<div class="gallerytext">caption</div>` — the detached `<figcaption>`
    // content, plus the optional `showfilename` filename link.
    let mut gallerytext = Node::element(ElementKind::Div);
    gallerytext.set_attr("class", "gallerytext");
    if opts.showfilename {
        gallerytext.push_child(showfilename_anchor(&title, config));
    }
    for node in caption_nodes {
        gallerytext.push_child(node);
    }
    if matches!(opts.mode.as_str(), "packed-overlay" | "packed-hover") {
        // Overlay/hover wrap the caption in a `.gallerytextwrapper` whose width
        // is `ceil(scaledWidth - 20)` (mirrors `PackedMode::galleryText`).
        let mut wrapper = Node::element(ElementKind::Div);
        wrapper.set_attr("class", "gallerytextwrapper");
        wrapper.set_attr(
            "style",
            format!("width: {}px;", (scale_width - 20.0).ceil() as u32),
        );
        wrapper.push_child(gallerytext);
        li.push_child(wrapper);
    } else {
        li.push_child(gallerytext);
    }

    Some(li)
}

/// Apply the packed/overlay/hover gallery's post-`AddMediaInfo` `scaleMedia`
/// step: read the rendered `<img>` width, divide by the mode's scale factor, and
/// re-stamp `width`/`height` on the media (mirrors `PackedMode::scaleMedia`).
/// Returns the unrounded scaled width used for `.thumb`/`.gallerybox` sizing;
/// non-packed modes return `opts.imageWidth` and leave the media untouched.
fn scale_media(figure: &mut Node, opts: &GalleryOpts) -> f64 {
    let scale = match opts.mode.as_str() {
        "packed" | "packed-overlay" | "packed-hover" => 1.5,
        _ => return opts.image_width as f64,
    };

    // The media element sits at `figure > a > <img|audio|span>`. A missing or
    // bad file leaves a broken `<span>` (no `width`), which is treated as a
    // non-numeric width (mirrors `scaleMedia`'s `is_numeric($width)` check).
    let media = figure
        .children
        .first_mut()
        .and_then(|a| a.children.first_mut());
    let Some(media) = media else {
        return opts.image_width as f64;
    };

    let is_audio =
        matches!(&media.kind, NodeKind::Element(ElementKind::Other(name)) if name == "audio");
    let width = media
        .get_attr("width")
        .and_then(|w| w.parse::<f64>().ok())
        .filter(|_| !is_audio);
    let scaled = match width {
        // Audio (or a broken span with no numeric width) gets the default
        // gallery width (mirrors `scaleMedia`'s `$opts->imageWidth` fallback).
        None => opts.image_width as f64,
        Some(w) => w / scale,
    };

    media.set_attr("width", (scaled.ceil() as u32).to_string());
    if is_audio {
        media.set_attr("style", format!("width: {scaled}px;"));
    }
    media.set_attr("height", opts.image_height.to_string());
    scaled
}

/// `thumbWidth` (mirrors `TraditionalMode::thumbWidth`/`PackedMode::thumbWidth`):
/// `width + padding.thumb`, with packed modes clamping to a minimum of 60px.
fn gallery_thumb_width(width: f64, padding: &Padding) -> f64 {
    let w = if padding.thumb == 0 {
        width.max(60.0)
    } else {
        width
    };
    w + padding.thumb as f64
}

/// Format a gallery `style` width like PHP's default `precision=14`
/// float-to-string: up to 10 fractional digits, trimming trailing zeros (so
/// `618.0` becomes `618`).
fn fmt_gallery_width(w: f64) -> String {
    let s = format!("{w:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// Detach the `<figcaption>` child from a media `<figure>`, returning its
/// children (the caption content). Mirrors `Gallery::pLine`'s figcaption
/// removal. When there is no figcaption (an inline `<span>` media), returns an
/// empty list.
fn take_figcaption(figure: &mut Node) -> Vec<Node> {
    let Some(idx) = figure
        .children
        .iter()
        .position(|c| matches!(c.kind, NodeKind::Element(ElementKind::FigCaption)))
    else {
        return Vec::new();
    };
    std::mem::take(&mut figure.children.remove(idx).children)
}

/// Convert a media `<figure>` (with its `<figcaption>` already removed) into an
/// inline `<span>`, migrating the remaining children (the `<a>`/broken-span) and
/// transferring `typeof`/`class`/`data-mw`/`data-parsoid`. The forced
/// `mw-halign-none` class from the `|none` option is dropped (mirrors
/// `TraditionalMode::line`).
fn figure_to_span(figure: &mut Node) -> Node {
    let mut span = Node::element(ElementKind::Span);
    // Transfer `typeof` (the media rdfa type, e.g. `mw:File`).
    if let Some(ty) = figure.get_attr("typeof").map(str::to_string) {
        span.set_attr("typeof", ty);
    }
    // Transfer `class`, dropping the horizontal-alignment marker from `|none`.
    if let Some(class) = figure.get_attr("class").map(str::to_string) {
        let filtered: Vec<&str> = class
            .split_whitespace()
            .filter(|c| !c.starts_with("mw-halign-"))
            .collect();
        if !filtered.is_empty() {
            span.set_attr("class", filtered.join(" "));
        }
    }
    span.data_mw = figure.data_mw.take();
    span.data_parsoid = figure.data_parsoid.take();
    span.dp = figure.dp.take();
    // Migrate the figure's children (the `<a>` + broken span).
    span.children = std::mem::take(&mut figure.children);
    span
}

/// The `<a class="galleryfilename galleryfilename-truncate">` link prepended by
/// the `showfilename` option (mirrors `Gallery::pLine`).
fn showfilename_anchor(title: &Title, config: &dyn SiteConfig) -> Node {
    let file = title.get_prefixed_text();
    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", crate::title::make_link(title, config));
    a.set_attr("class", "galleryfilename galleryfilename-truncate");
    a.set_attr("title", file.as_str());
    a.push_child(Node::text(file));
    a
}

/// Parse the `<gallery …>` start-tag attributes into options.
fn parse_opts(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
) -> GalleryOpts {
    let mut opts = GalleryOpts::default();
    let attrs = crate::pipeline::extension_handler::extension_kv_attrs(token);

    for kv in &attrs {
        let key = kv.key.as_str().unwrap_or_default();
        let val = kv.value.as_str().unwrap_or_default();
        match key {
            "widths" => {
                // Mirrors `Opts::__construct`: `parseMediaDimensions(..., true, false)`.
                // The `true` means only a single dimension (the width) is accepted,
                // i.e. `x` is present in `100x50` → `null`. The localized width suffix
                // (`px`/`ra`) is stripped by the `img_width` parameterized alias matcher.
                if let Some(w) = parse_gallery_dimension(config, val, true) {
                    opts.image_width = w;
                }
            }
            "heights" => {
                if let Some(h) = parse_gallery_dimension(config, val, true) {
                    opts.image_height = h;
                }
            }
            "perrow" => {
                if let Ok(n) = val.parse::<u32>() {
                    opts.images_per_row = n;
                }
            }
            "mode" => {
                let mode = val.to_lowercase();
                if mode == "traditional"
                    || mode == "nolines"
                    || mode == "packed"
                    || mode == "packed-overlay"
                    || mode == "packed-hover"
                    || mode == "slideshow"
                {
                    opts.mode = mode;
                } else {
                    // Unknown mode → traditional (mirrors `Mode::byName`).
                    opts.mode = "traditional".to_string();
                }
            }
            "showfilename" => opts.showfilename = true,
            // `showthumbnails` (slideshow mode) is a presence flag; its value
            // (often the empty string) is irrelevant (mirrors `Opts`'s `isset`).
            "showthumbnails" => opts.showthumbnails = true,
            // `showfilenames` (plural) is NOT a recognized gallery option; it is
            // preserved in `data-mw` only and does not enable filename links nor
            // become a `<ul>` attribute (mirrors PHP `Opts`, which keys on the
            // singular `showfilename`).
            "showfilenames" => {}
            // `caption` is rendered as a leading `<li class="gallerycaption">`.
            "caption" => {
                if !val.is_empty() {
                    opts.caption = val.to_string();
                }
            }
            // `summary` is a legacy gallery attribute with no HTML5 `ul`
            // equivalent; it is recorded in `data-mw` only (not emitted).
            "summary" => {}
            // `class`, `style`, `data-test`, `type`, etc. → the <ul>.
            other => opts.attrs.push((other.to_string(), val.to_string())),
        }
    }

    let _ = config;
    opts
}

/// Parse a gallery `widths`/`heights` value following PHP's
/// `Utils::parseMediaDimensions(siteConfig, str, onlyOne=true, localized=false)`.
///
/// 1. The localized `px`/`ra` suffix is stripped by matching the `img_width`
///    parameterized alias (via `get_option_info`), yielding the bare numeric
///    string. (`100ra` → `100`, `120px` → `120`.)
/// 2. The remaining string is matched against `^(\d*)(?:x(\d+))?\s*(px\s*)?$`;
///    with `onlyOne=true`, an `x` (height) portion makes the result `null`.
/// 3. `validateMediaParam` requires the value to be `> 0`.
fn parse_gallery_dimension(config: &dyn SiteConfig, s: &str, only_one: bool) -> Option<u32> {
    use crate::pipeline::media_options::get_option_info;

    // Step 1: strip the localized width/height suffix. `img_width` is registered
    // with aliases like `$1px` and (for `eo`) `$1ra`; `get_option_info` matches the
    // parameterized alias and captures the `$1` value.
    let bare = match get_option_info(config, s) {
        Some(info) if info.ck == "width" => info.v,
        _ => s.trim().to_string(),
    };

    // Step 2: `$str` is now the bare numeric string (still possibly with a
    // trailing `px`; the `px` is consumed either by the alias match above or by
    // the regex below). Mirror the regex `^(\d*)(?:x(\d+))?\s*(px\s*)?$`.
    let s = bare.trim();
    let s = s.strip_suffix("px").unwrap_or(s);
    let s = s.trim();

    let mut parts = s.split('x');
    let x_str = parts.next()?.trim();
    let x: u32 = x_str.parse().ok()?;
    if only_one && parts.next().is_some() {
        // An explicit height is present; `onlyOne` rejects multi-dimensional input.
        return None;
    }
    // Step 3: `validateMediaParam` (`> 0`).
    if x == 0 {
        return None;
    }
    Some(x)
}

/// Render gallery caption wikitext into a list of inline DOM nodes, following
/// wikilinks/external-URL autolinks the same way the main pipeline does.
/// Mirrors `renderMedia`'s caption handling (`processContentInPipeline` with
/// `inlineContext => true`) — shared with `Parser::build_inline_fragment`.
fn caption_to_nodes(caption: &str, config: &dyn SiteConfig) -> Vec<Node> {
    use crate::wikitext::tokenizer_v2::{PegTokenizer, TokenizerOptions};
    use crate::wikitext::tokens_v2::{Either, Item, ParsoidToken};

    // Tokenize the caption (quotes, entities, urllink/extlink/wikilink tokens).
    let options = TokenizerOptions {
        magic_links: crate::wikitext::tokenizer_v2::MagicLinkConfig {
            rfc: config.magic_link_enabled("RFC"),
            pmid: config.magic_link_enabled("PMID"),
            isbn: config.magic_link_enabled("ISBN"),
        },
        ext_tags: config.extension_tags().to_vec(),
        protocols: config.protocols().iter().map(|s| s.to_string()).collect(),
        ..TokenizerOptions::default()
    };
    let mut tokenizer = PegTokenizer::new(caption, &options);
    let chunks = tokenizer.tokenize().unwrap_or_default();

    let items: Vec<Item> = chunks
        .into_iter()
        .map(|chunk| match chunk {
            Either::Left(s) => Item::Str(s),
            Either::Right(t) => match t {
                ParsoidToken::SelfclosingTag(stt) => Item::Tok(ParsoidToken::SelfclosingTag(stt)),
                other => Item::Tok(other),
            },
        })
        .collect();

    let mut fragments = std::collections::HashMap::new();
    let mut next_id = 0usize;
    let frag = crate::pipeline::parser::render_inline_fragment(
        config,
        items,
        &mut fragments,
        &mut next_id,
    );
    frag.children
}

/// Append a value to an attribute (mirrors `TraditionalMode::appendAttr`).
fn append_attr(node: &mut Node, key: &str, value: &str) {
    if let Some(existing) = node.get_attr(key) {
        let merged = if existing.trim().is_empty() {
            value.to_string()
        } else {
            format!("{existing} {value}")
        };
        node.set_attr(key, merged);
    } else {
        node.set_attr(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wikitext::tokens_v2::{DataParsoid, SelfclosingTagTk};

    /// Build a `<gallery>` extension token with the given body.
    fn gallery_token(source: &str) -> SelfclosingTagTk {
        let mut tk = SelfclosingTagTk::new("extension", vec![], DataParsoid::default());
        tk.add_attribute_str("name", "gallery");
        tk.add_attribute_str("source", source);
        tk.data_parsoid.ext_tag_offsets = Some(crate::wikitext::tokens_v2::DomSourceRange {
            start: Some(0),
            end: Some(source.len()),
            open_width: Some("<gallery>".len()),
            close_width: Some("</gallery>".len()),
        });
        tk
    }

    #[test]
    fn test_gallery_builds_ul() {
        let config = crate::mock::MockSiteConfig::new();
        let token = gallery_token("<gallery>\nFile:Foobar.jpg|caption\n</gallery>");
        let ul = build(&token, &config);
        assert_eq!(ul.get_attr("class"), Some("gallery mw-gallery-traditional"));
        assert_eq!(ul.get_attr("typeof"), Some("mw:Extension/gallery"));
        assert!(!ul.children.is_empty());
        // First child is a gallerybox li.
        assert!(matches!(
            ul.children[0].kind,
            crate::dom::node::NodeKind::Element(crate::dom::node::ElementKind::ListItem)
        ));
    }
}
