//! `<gallery>` extension — faithful port of PHP Parsoid's
//! `Ext/Gallery` (traditional mode) for the wt2html direction.
//!
//! Implements the subset of `Gallery::sourceToDom`/`TraditionalMode::render` that
//! the parser-test fixtures exercise: traditional mode with `widths`/`heights`/
//! `perrow`/`caption`/`mode`/`showfilename`/`class`/`style` options. Other modes
//! (nolines, slideshow, packed, packed-overlay, packed-hover) are deferred.

use crate::dom::node::{ElementKind, Node};
use crate::title::{Title, TitleParser};
use crate::traits::SiteConfig;

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

/// Build the `<ul …>` fragment, rendering every caption through `render_caption`
/// (returning a future of the rendered inline nodes). `build`/`build_with_sync`
/// use the plain [`caption_to_nodes`] renderer; the async parser path provides a
/// renderer that additionally expands caption templates (mirrors PHP's
/// `Gallery::pCaption`/`renderMedia` running the caption through
/// `processContentInPipeline` with `expandTemplates => true`).
pub async fn build_with<F, Fut>(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
    mut render_caption: F,
) -> Node
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = Vec<Node>>,
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
        if let Some(li) = render_line_with(&opts, line, config, &mut render_caption).await {
            ul.push_child(li);
        }
    }

    ul
}

/// Synchronous [`build_with`] entry point (used by the `wikitext_to_ast` path
/// and tests): render each caption through [`caption_to_nodes`] and block on the
/// (immediately-ready) future.
pub fn build_with_sync(
    token: &crate::wikitext::tokens_v2::SelfclosingTagTk,
    config: &dyn SiteConfig,
) -> Node {
    use std::task::{Context, Poll, Waker};

    let fut = build_with(token, config, |caption: &str| {
        std::future::ready(caption_to_nodes(caption, config))
    });
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(node) => node,
        // `caption_to_nodes` returns a `ready` future, so this branch is
        // unreachable in practice.
        Poll::Pending => unreachable!("gallery caption future is always ready"),
    }
}

/// [`render_line`] with a caller-supplied caption renderer (see [`build_with`]).
async fn render_line_with<F, Fut>(
    opts: &GalleryOpts,
    line: &str,
    config: &dyn SiteConfig,
    render_caption: &mut F,
) -> Option<Node>
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = Vec<Node>>,
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
    let rest = if rest_str.is_empty() {
        None
    } else {
        Some(rest_str.as_str())
    };

    // Title resolution: decode entities (`&#45;` → `-`), mirroring
    // `Gallery::pLine`'s `Utils::decodeWtEntities($oTitleStr)`.
    let file_ns = config.canonical_namespace_id("File").unwrap_or(6);
    let decoded = crate::html::wts_utils::decode_wt_entities_all(&title_str.replace("_", " "));
    // A title with illegal characters (e.g. `[[x`) is rejected, mirroring
    // `makeTitle` returning null in `Gallery::pLine` (the line is then dropped).
    if crate::title::has_invalid_chars(&decoded) {
        return None;
    }
    let mut title = TitleParser::parse(&decoded, config);
    if title.namespace_id != file_ns {
        // Re-parse with an explicit `File:` prefix so first-letter capitalization
        // (ucfirst) is applied to the title text (mirrors `renderMedia`'s
        // `makeTitle( $decodedTitleStr, $fileNs )`).
        title = TitleParser::parse(&format!("File:{decoded}"), config);
    }
    if title.namespace_id != file_ns {
        return None;
    }

    // Caption: split the remainder on `|`, skipping recognized media options
    // (`300px`, `centre`, `link=…`, …). The last *unrecognized* non-empty segment
    // is the caption (mirrors `renderMedia`'s option parsing, where recognized
    // options are consumed and only the final non-option becomes the caption).
    let caption = rest.and_then(|r| {
        r.split('|')
            .rev()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .find(|seg| crate::pipeline::media_options::get_option_info(config, seg).is_none())
            .map(str::to_string)
    });

    // Also parse the non-caption options (`link=`, `alt=`, `manualthumb=`) into
    // `data-mw.attribs` so `AddMediaInfo` applies them (mirrors `renderFile`'s
    // `$dataMw->attribs`). The gallery thumbnails are regenerated per-line.
    let media_opts = parse_media_opts(rest, config);

    let has_error = false;

    // Thumbnail dims: thumbWidth = imageWidth + padding.thumb, thumbHeight =
    // imageHeight + padding.thumb, boxWidth = thumbWidth + padding.box.
    let padding = padding_for_mode(&opts.mode);
    let thumb_width = opts.image_width + padding.thumb;
    let thumb_height = opts.image_height + padding.thumb;
    let box_width = thumb_width + padding.box_padding;

    // `<li class="gallerybox" style="width: <boxWidth>px;">`
    let mut li = Node::element(ElementKind::ListItem);
    li.set_attr("class", "gallerybox");
    li.set_attr("style", format!("width: {box_width}px;"));
    li.data_mw = Some("{}".to_string());

    // `<div class="thumb" style="...">`
    let mut thumb = Node::element(ElementKind::Div);
    thumb.set_attr("class", "thumb");
    // The `height` is only emitted on error or for `traditional` mode; the
    // nolines/slideshow/packed modes omit it (mirrors `TraditionalMode::thumbStyle`).
    let thumb_style = if has_error {
        format!("height: {thumb_height}px;")
    } else if opts.mode == "traditional" {
        format!("width: {thumb_width}px; height: {thumb_height}px;")
    } else {
        format!("width: {thumb_width}px;")
    };
    thumb.set_attr("style", thumb_style);

    // Broken-media span (mirrors `renderFile`, resolved later by AddMediaInfo).
    thumb.push_child(broken_media_span(
        &title,
        opts,
        config,
        &media_opts,
        caption.as_deref(),
    ));

    li.push_child(thumb);

    // `<div class="gallerytext">caption</div>` — the caption is rendered as
    // wikitext (external-URL autolinks, wikilinks, quotes, …).
    let mut gallerytext = Node::element(ElementKind::Div);
    gallerytext.set_attr("class", "gallerytext");
    // `showfilename` prepends a filename link (mirrors `Gallery::pLine`).
    if opts.showfilename {
        gallerytext.push_child(showfilename_anchor(&title, config));
    }
    if let Some(cap) = &caption {
        for node in render_caption(cap).await {
            gallerytext.push_child(node);
        }
    }
    li.push_child(gallerytext);

    Some(li)
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

/// Build the broken-media `<span typeof="mw:File">` inside a gallery thumb. This
/// is the same structure `renderFile` emits (a red link + broken span), which
/// `AddMediaInfo` then resolves into an `<img>` (or `mw:Error` for missing files).
/// `media_opts` carries the non-caption `link=`/`alt=`/`manualthumb=` options as
/// `data-mw.attribs` (mirrors `renderFile`'s `$dataMw->attribs`).
fn broken_media_span(
    title: &Title,
    opts: &GalleryOpts,
    config: &dyn SiteConfig,
    media_opts: &crate::pipeline::media_options::MediaOpts,
    caption: Option<&str>,
) -> Node {
    let mut span = Node::element(ElementKind::Span);
    span.set_attr("class", "mw-file-element mw-broken-media");
    span.set_attr("resource", crate::title::make_link(title, config));
    span.set_attr("data-width", opts.image_width.to_string());
    span.set_attr("data-height", opts.image_height.to_string());
    // `lang=` is a global attribute applied to the broken span (mirrors
    // `renderFile`), read back by `AddMediaInfo::lang_from_container` to build
    // the `?lang=` description-link query.
    if let Some(lang) = &media_opts.lang {
        span.set_attr("lang", lang);
    }
    span.push_child(Node::text(title.get_prefixed_text()));

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", config.get_upload_url(&title.get_dbkey()));
    a.set_attr("class", "new");
    a.set_attr("title", title.get_prefixed_text());
    a.push_child(span);

    let mut container = Node::element(ElementKind::Span);
    if media_opts.expanded_attrs {
        container.set_attr("typeof", "mw:File mw:ExpandedAttrs");
    } else {
        container.set_attr("typeof", "mw:File");
    }
    // A `class=` option is applied to the wrapper (mirrors `renderFile`, where
    // the user class is appended to the container's class list).
    if let Some(class) = &media_opts.class
        && !class.trim().is_empty()
    {
        container.set_attr("class", class.as_str());
    }
    if let Some(data_mw) = media_opts_to_data_mw(media_opts, caption) {
        container.data_mw = Some(data_mw);
    }
    container.push_child(a);
    container
}

/// The media options that influence `AddMediaInfo`/`rewrite_structure` (the
/// gallery subset of `renderFile`'s `data-mw.attribs`).
fn parse_media_opts(
    rest: Option<&str>,
    config: &dyn SiteConfig,
) -> crate::pipeline::media_options::MediaOpts {
    use crate::pipeline::media_options::{MediaOpts, get_option_info};

    let Some(r) = rest else {
        return MediaOpts::default();
    };
    let mut opts = MediaOpts::default();
    for part in crate::pipeline::wiki_link_render::split_media_options(r) {
        let Some(info) = get_option_info(config, part.trim()) else {
            continue;
        };
        match info.ck.as_str() {
            "manualthumb" => opts.manualthumb = Some(info.v),
            "link" => {
                opts.link = Some(crate::pipeline::media_options::strip_quote_markers(&info.v))
            }
            "alt" => {
                opts.expanded_attrs |= crate::pipeline::media_options::has_wikitext_markup(&info.v);
                opts.alt = Some(crate::pipeline::media_options::strip_quote_markers(&info.v));
            }
            "class" => opts.class = Some(info.v),
            "lang" if crate::pipeline::media_options::is_valid_internal_lang(&info.v) => {
                opts.lang = Some(info.v)
            }
            _ => {}
        }
    }
    opts
}

/// Serialize the gallery media options into a `data-mw` JSON string carrying an
/// `attribs` array and (when present) a `caption` string (mirrors `renderFile`'s
/// `dataMw->attribs` + inline-media `dataMw->caption`), or `None` when there is
/// nothing to store.
fn media_opts_to_data_mw(
    opts: &crate::pipeline::media_options::MediaOpts,
    caption: Option<&str>,
) -> Option<String> {
    use crate::wikitext::tokens_v2::{DataMwAttrib, DataMwValue};

    let mut attribs: Vec<DataMwAttrib> = Vec::new();
    for (key, val) in [
        ("link", opts.link.as_ref()),
        ("alt", opts.alt.as_ref()),
        ("manualthumb", opts.manualthumb.as_ref()),
    ] {
        if let Some(v) = val {
            attribs.push(DataMwAttrib::new(
                DataMwValue::Str(key.to_string()),
                DataMwValue::Object {
                    txt: Some(v.clone()),
                    html: None,
                    uneditable: false,
                },
            ));
        }
    }
    if attribs.is_empty() && caption.is_none() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    if !attribs.is_empty() {
        let json = crate::pipeline::attribute_expander::serialize_data_mw_attribs(&attribs);
        obj.insert(
            "attribs".to_string(),
            serde_json::from_str(&json).unwrap_or(serde_json::Value::Array(vec![])),
        );
    }
    if let Some(cap) = caption {
        obj.insert(
            "caption".to_string(),
            serde_json::Value::String(cap.to_string()),
        );
    }
    Some(serde_json::Value::Object(obj).to_string())
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
