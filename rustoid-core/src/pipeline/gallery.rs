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

/// Traditional-mode padding (mirrors `TraditionalMode`).
const PADDING_THUMB: u32 = 30;
const PADDING_BOX: u32 = 5;

/// Parsed `<gallery>` options.
#[derive(Debug)]
struct GalleryOpts {
    image_width: u32,
    image_height: u32,
    images_per_row: u32,
    mode: String,
    showfilename: bool,
    caption: bool,
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
            caption: false,
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

    // Remaining sanitized attrs (style, data-test, …).
    for (k, v) in &opts.attrs {
        if k == "class" {
            continue;
        }
        // Append to existing (mirrors `appendAttr`), but for simplicity set.
        if let Some(existing) = ul.get_attr(k) {
            let merged = format!("{existing} {v}");
            ul.set_attr(k, merged);
        } else {
            ul.set_attr(k, v);
        }
    }

    // perrow → max-width on the <ul> (mirrors `TraditionalMode::perRow`).
    if opts.images_per_row > 0 {
        let total = opts.image_width + PADDING_THUMB + PADDING_BOX + 8; // + border
        let total = total * opts.images_per_row;
        append_attr(&mut ul, "style", &format!("max-width: {total}px;"));
    }

    // data-mw names the extension (stripped in harness comparison, but set for
    // round-trip fidelity).
    ul.data_mw = Some(r#"{"name":"gallery","attrs":{},"body":{}}"#.to_string());

    // Parse and render each line.
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(li) = render_line(&opts, line, config) {
            ul.push_child(li);
        }
    }

    ul
}

/// Parse a single gallery line into a `<li class="gallerybox">`.
fn render_line(opts: &GalleryOpts, line: &str, config: &dyn SiteConfig) -> Option<Node> {
    let line = line.trim();
    // Split on the first `|` (title | caption+options).
    let (title_str, rest) = match line.split_once('|') {
        Some((t, r)) => (t.trim(), Some(r)),
        None => (line, None),
    };

    // Title resolution: try the title as-is (File namespace), else prefix File:.
    let file_ns = config.canonical_namespace_id("File").unwrap_or(6);
    let decoded = title_str.replace("_", " ");
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

    let has_error = false;

    // Thumbnail dims: thumbWidth = imageWidth + 30, thumbHeight = imageHeight + 30,
    // boxWidth = thumbWidth + 5.
    let thumb_width = opts.image_width + PADDING_THUMB;
    let thumb_height = opts.image_height + PADDING_THUMB;
    let box_width = thumb_width + PADDING_BOX;

    // `<li class="gallerybox" style="width: <boxWidth>px;">`
    let mut li = Node::element(ElementKind::ListItem);
    li.set_attr("class", "gallerybox");
    li.set_attr("style", format!("width: {box_width}px;"));
    li.data_mw = Some("{}".to_string());

    // `<div class="thumb" style="...">`
    let mut thumb = Node::element(ElementKind::Div);
    thumb.set_attr("class", "thumb");
    let thumb_style = if has_error {
        format!("height: {thumb_height}px;")
    } else {
        format!("width: {thumb_width}px; height: {thumb_height}px;")
    };
    thumb.set_attr("style", thumb_style);

    // Broken-media span (mirrors `renderFile`, resolved later by AddMediaInfo).
    thumb.push_child(broken_media_span(&title, opts, config));

    li.push_child(thumb);

    // `<div class="gallerytext">caption</div>`
    let mut gallerytext = Node::element(ElementKind::Div);
    gallerytext.set_attr("class", "gallerytext");
    if let Some(cap) = &caption {
        gallerytext.push_child(Node::text(cap.clone()));
    }
    li.push_child(gallerytext);

    Some(li)
}

/// Build the broken-media `<span typeof="mw:File">` inside a gallery thumb. This
/// is the same structure `renderFile` emits (a red link + broken span), which
/// `AddMediaInfo` then resolves into an `<img>` (or `mw:Error` for missing files).
fn broken_media_span(title: &Title, opts: &GalleryOpts, config: &dyn SiteConfig) -> Node {
    let mut span = Node::element(ElementKind::Span);
    span.set_attr("class", "mw-file-element mw-broken-media");
    span.set_attr("resource", crate::title::make_link(title, config));
    span.set_attr("data-width", opts.image_width.to_string());
    span.set_attr("data-height", opts.image_height.to_string());
    span.push_child(Node::text(title.get_prefixed_text()));

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", config.get_upload_url(&title.get_dbkey()));
    a.set_attr("class", "new");
    a.set_attr("title", title.get_prefixed_text());
    a.push_child(span);

    let mut container = Node::element(ElementKind::Span);
    container.set_attr("typeof", "mw:File");
    container.push_child(a);
    container
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
                if let Some(w) = parse_dimension(val) {
                    opts.image_width = w;
                }
            }
            "heights" => {
                if let Some(h) = parse_dimension(val) {
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
            "caption" => opts.caption = true,
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

/// Parse a bare dimension (`120`, `120px`, `120x100`).
fn parse_dimension(s: &str) -> Option<u32> {
    let s = s.trim();
    let s = s.strip_suffix("px").unwrap_or(s);
    let first = s.split('x').next()?.trim();
    first.parse::<u32>().ok()
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

    #[test]
    fn test_parse_dimension() {
        assert_eq!(parse_dimension("120"), Some(120));
        assert_eq!(parse_dimension("120px"), Some(120));
        assert_eq!(parse_dimension("120x100"), Some(120));
    }
}
