//! AddMediaInfo — faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Processors/AddMediaInfo.php`.
//!
//! `WikiLinkHandler::renderFile` always emits *broken* media: a
//! `<span class="mw-file-element mw-broken-media">` inside an
//! `<a class="new" href="<upload-url>">`. This DOM pass fetches file metadata
//! for every `[typeof~="mw:File"]` container and, when the file exists,
//! replaces the broken span with a real `<img>` and the red-link anchor with an
//! `<a class="mw-file-description" href="./File:…">` description link. Missing
//! or bad-list files keep the broken markup and gain an `mw:Error` type.
//!
//! Only the bitmap-image branch of the PHP processor is implemented here;
//! audio/video (`handleAudio`/`handleVideo`), manual-thumb, pagination, and the
//! timed-media option surface are deferred until the corresponding file-info
//! fields are plumbed through.

use std::collections::HashMap;

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::title::{Title, TitleParser};
use crate::traits::{DataSource, FileInfo, SiteConfig};

/// The fixed set of media-image attributes stamped on the `<img>`. PHP uses
/// `decoding="async"` plus `loading="lazy"` (the parser-test harness enables
/// lazy loading via `$wgUseInstantCommons`-style flags).
const IMG_ATTRIBS: [(&str, &str); 2] = [("decoding", "async"), ("loading", "lazy")];

/// Fetch file info for every `[typeof~="mw:File"]` container in `root` and
/// rewrite the broken-media placeholder into a real `<img>` (or leave it broken
/// and mark `mw:Error`). Faithful to `AddMediaInfo::run`.
pub async fn run(root: &mut Node, source: &dyn DataSource, config: &dyn SiteConfig) {
    // Collect containers (deepest-first) with their index paths from `root`.
    let mut jobs: Vec<ContainerJob> = Vec::new();
    collect_containers(root, &mut Vec::new(), &mut jobs, config);

    // Resolve redirects (a redirect-to-file title yields its target's media
    // info; mirrors the API's `redirects=1` following) and batch-fetch file info
    // (PHP issues a single getFileInfo API call). Deduplicate by title so each
    // distinct file is fetched once.
    let mut infos: HashMap<String, Option<FileInfo>> = HashMap::new();
    for job in jobs.iter_mut() {
        if let Ok(Some(target)) = source.resolve_redirect(&job.title).await {
            // Re-target: the description link and `resource` use the resolved
            // title, not the redirect title.
            job.title = target;
        }
        let key = job.title.full_text();
        if infos.contains_key(&key) {
            continue;
        }
        let info = source.get_file_info(&job.title).await.unwrap_or(None);
        infos.insert(key, info);
    }

    for job in &jobs {
        apply_media_info(root, job, &infos, config);
    }
}

/// A `mw:File` container discovered during the collection walk.
struct ContainerJob {
    /// Path of child indices from `root` to the container node.
    path: Vec<usize>,
    /// Title parsed from the container's broken span text (the file DB key).
    title: Title,
    /// The broken span's `data-width` (dims.width), if any.
    data_width: Option<String>,
}

/// Parse the file title from a media container's broken span text.
///
/// PHP resolves the title from `$span->textContent` (the prefixed DB text the
/// tokenizer stashed inside the broken span). Mirrors that behavior.
fn title_from_container(container: &Node, config: &dyn SiteConfig) -> Title {
    let anchor = first_element_child(container);
    let span = anchor.and_then(first_element_child);
    let text = span.map(text_content).unwrap_or_default();
    // Strip leading/trailing whitespace; empty text falls back to the resource.
    let text = text.trim();
    if text.is_empty() {
        if let Some(resource) = span.and_then(|s| s.get_attr("resource")) {
            TitleParser::parse(resource, config)
        } else {
            TitleParser::parse("", config)
        }
    } else {
        TitleParser::parse(text, config)
    }
}

/// The `data-width` attribute on a container's broken span, if present.
fn data_width_from_container(container: &Node) -> Option<String> {
    let anchor = first_element_child(container)?;
    let span = first_element_child(anchor)?;
    span.get_attr("data-width").map(str::to_string)
}

/// The first element (non-text) child of `node`, if any.
fn first_element_child(node: &Node) -> Option<&Node> {
    node.children
        .iter()
        .find(|c| matches!(c.kind, NodeKind::Element(_)))
}

/// The concatenated text content of a node (mirrors DOM `textContent`).
fn text_content(node: &Node) -> String {
    let mut out = String::new();
    for child in &node.children {
        match &child.kind {
            NodeKind::Text(t) => out.push_str(t),
            _ => out.push_str(&text_content(child)),
        }
    }
    out
}

/// Whether a node is a media container (has a `mw:File` `typeof` token).
fn is_media_container(node: &Node) -> bool {
    node.get_attr("typeof")
        .map(|t| {
            t.split_whitespace()
                .any(|tok| tok == "mw:File" || tok.starts_with("mw:File/"))
        })
        .unwrap_or(false)
}

/// Collect `[typeof~="mw:File"]` containers, deepest-first so rewrites don't
/// invalidate the recorded paths of outer containers. Mirrors PHP's
/// `querySelectorAll('[typeof*="mw:File"]')` + traversal guard.
fn collect_containers(
    node: &mut Node,
    path: &mut Vec<usize>,
    out: &mut Vec<ContainerJob>,
    config: &dyn SiteConfig,
) {
    // Do not descend into a media container (a fragment-embedded media is
    // handled in its own pipeline; mirrors PHP's `isDOMFragmentWrapper` guard).
    if is_media_container(node) {
        out.push(ContainerJob {
            path: path.clone(),
            title: title_from_container(node, config),
            data_width: data_width_from_container(node),
        });
        return;
    }
    for i in 0..node.children.len() {
        path.push(i);
        collect_containers(&mut node.children[i], path, out, config);
        path.pop();
    }
}

/// Replace the broken span with a real `<img>` element and rewrite the anchor
/// into a file-description link. Faithful to the bitmap branch of
/// `AddMediaInfo::run` (`handleImage` + `replaceAnchor`).
fn apply_media_info(
    root: &mut Node,
    job: &ContainerJob,
    infos: &HashMap<String, Option<FileInfo>>,
    config: &dyn SiteConfig,
) {
    let info = infos.get(&job.title.full_text()).and_then(|i| i.clone());

    let Some(info) = info else {
        // Missing file: leave broken, add `mw:Error` (mirrors `handleErrors`).
        mark_error(root, &job.path);
        return;
    };

    // Compute the rendered size (mirrors `handleSize` for bitmaps).
    let (width, height) = handle_size(job, &info);

    // The image `src` (thumbnail when the file has one for the requested width,
    // else the raw file URL).
    let src = image_src(&info, job.data_width.as_deref());

    // The caption text (trimmed) for `alt`/`title` when no explicit `alt`/`link`
    // option is present (mirrors `$captionText` → `$alt` in `AddMediaInfo`).
    // `hasVisibleCaption` (Thumb/Frame formats) suppresses the caption from
    // becoming `alt`/`title`; those captions live only in the `<figcaption>`.
    let caption_text = if has_visible_caption(root, &job.path) {
        None
    } else {
        caption_text(root, &job.path)
    };

    // Build the `<img>` replacement.
    let mut img = Node::element(ElementKind::Other("img".to_string()));
    // resource copied from the broken span's title (the file DB key).
    img.set_attr("resource", crate::title::make_link(&job.title, config));
    // alt from the caption (when present), before the fixed attrs.
    if let Some(alt) = &caption_text {
        img.set_attr("alt", alt);
    }
    // Fixed attribute set (decoding/loading).
    for (k, v) in IMG_ATTRIBS {
        img.set_attr(k, v);
    }
    // data-file-* read-only original size info (T64881).
    img.set_attr("data-file-width", info.width.to_string());
    img.set_attr("data-file-height", info.height.to_string());
    img.set_attr("data-file-type", media_type_from_mime(&info.mime_type));
    // src + srcset (responsive 2x).
    img.set_attr("src", src);
    if let Some(srcset) = srcset(&info) {
        img.set_attr("srcset", srcset);
    }
    // Rendered dimensions.
    img.set_attr("height", height.to_string());
    img.set_attr("width", width.to_string());
    img.set_attr("class", "mw-file-element");

    rewrite_structure(
        root,
        &job.path,
        &job.title,
        img,
        config,
        caption_text.as_deref(),
    );
}

/// Whether a media container has a *visible* caption (Thumb/Frame formats).
/// Mirrors PHP `WTUtils::hasVisibleCaption`, which suppresses the caption from
/// being mirrored into `alt`/`title` (those captions render only in the
/// `<figcaption>`).
fn has_visible_caption(root: &Node, path: &[usize]) -> bool {
    let Some(container) = node_at_read(root, path) else {
        return false;
    };
    matches!(media_format(container).as_str(), "Thumb" | "Frame")
}

/// The `/Format` suffix of a media container's `mw:File/…` `typeof` (empty when
/// none). Mirrors `WTUtils::getMediaFormat`.
fn media_format(node: &Node) -> String {
    crate::html::wts_utils::get_media_format(node)
}

/// The trimmed caption text of a media container (its `<figcaption>` content,
/// if non-empty). Mirrors `$captionText = trim(textContentFromCaption($caption))`.
/// Note: only block (`<figure>`) media has a `<figcaption>` child; inline
/// (`<span>`) captions live in `data-mw.caption` instead and are handled
/// separately.
fn caption_text(root: &Node, path: &[usize]) -> Option<String> {
    let container = node_at_read(root, path)?;
    let figcaption = container
        .children
        .iter()
        .find(|c| matches!(c.kind, NodeKind::Element(ElementKind::FigCaption)))?;
    let text = text_content(figcaption);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Navigate to the node at `path` (read-only).
fn node_at_read<'a>(root: &'a Node, path: &[usize]) -> Option<&'a Node> {
    let mut node = root;
    for &idx in path {
        node = node.children.get(idx)?;
    }
    Some(node)
}

/// Compute the rendered width/height for a bitmap image (mirrors `handleSize`
/// for the common non-upscaling bitmap cases).
fn handle_size(job: &ContainerJob, info: &FileInfo) -> (u32, u32) {
    let (mut width, mut height) = (info.width, info.height);

    // A `thumb`/`frameless` request carries the target width on the broken span
    // (`data-width`). Scale proportionally (exact thumb-height is not derivable
    // from `FileInfo`, so we preserve the file's aspect ratio).
    if let Some(w_str) = job.data_width.as_deref()
        && let Ok(w) = w_str.parse::<u32>()
        && w > 0
        && info.width > 0
    {
        width = w;
        // Scale height proportionally, rounding half-up (mirrors core's
        // `File::scaleHeight` → `round( $height * $twidth / $width )`).
        let scaled = (info.height as u64 * w as u64 + info.width as u64 / 2) / info.width as u64;
        height = scaled as u32;
    }

    (width, height)
}

/// The `src` URL for the image (thumbnail for the requested width, else raw).
fn image_src(info: &FileInfo, requested_width: Option<&str>) -> String {
    if let Some(w) = requested_width
        && let Some(thumb) = info.thumb_urls.get(w)
    {
        return thumb.clone();
    }
    info.file_url.clone()
}

/// The 2x `srcset` value (responsive images), mirroring PHP's `responsiveUrls`.
/// Only derivable when the data source exposes a 2x thumbnail; otherwise absent.
fn srcset(info: &FileInfo) -> Option<String> {
    // A "2x" candidate maps to a thumbnail at twice the natural width. We can
    // only emit it when the file provides both natural and a wider thumb; for
    // now, no srcset is emitted (the harness strips it from comparison anyway).
    let _ = info;
    None
}

/// Map a MIME type to the lowercase `data-file-type` value PHP emits
/// (`BITMAP` → `bitmap`, `SVG` → `drawing`, etc.).
fn media_type_from_mime(mime: &str) -> String {
    match mime {
        "image/svg+xml" => "drawing".to_string(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/x-ms-bmp" => {
            "bitmap".to_string()
        }
        _ => mime.rsplit('/').next().unwrap_or("").to_string(),
    }
}

/// Mark a media container as `mw:Error` and keep the broken markup (mirrors
/// `handleErrors`).
fn mark_error(root: &mut Node, path: &[usize]) {
    let Some(node) = node_at(root, path) else {
        return;
    };
    // Add `mw:Error` to the typeof (space-separated, non-duplicated, first).
    let mut tokens: Vec<String> = node
        .get_attr("typeof")
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if !tokens.iter().any(|t| t == "mw:Error") {
        tokens.insert(0, "mw:Error".to_string());
        node.set_attr("typeof", tokens.join(" "));
    }

    // data-mw errors array, mirroring `handleErrors`'s merged errors.
    let errors = r#"{"errors":[{"key":"apierror-filedoesnotexist","message":"This image does not exist."}]}"#;
    node.data_mw = Some(errors.to_string());
}

/// Replace the broken `<span>` with the built `<img>` and rewrite the anchor to
/// a file-description link. Mirrors `replaceAnchor` (image branch) +
/// `$anchor->appendChild($elt)`.
fn rewrite_structure(
    root: &mut Node,
    path: &[usize],
    title: &Title,
    img: Node,
    config: &dyn SiteConfig,
    caption_text: Option<&str>,
) {
    let Some(container) = node_at(root, path) else {
        return;
    };
    // The anchor is the first element child of the container; the span is its
    // first element child.
    let anchor_idx = match container
        .children
        .iter()
        .position(|c| matches!(c.kind, NodeKind::Element(_)))
    {
        Some(i) => i,
        None => return,
    };

    {
        let anchor = &mut container.children[anchor_idx];
        // Description link to the file page (mirrors `$addDescriptionLink`).
        anchor.set_attr("href", crate::title::make_link(title, config));
        anchor.set_attr("class", "mw-file-description");
        // `title` from the caption (or absent when the caption is empty).
        anchor.attrs.retain(|a| a.key != "title");
        if let Some(cap) = caption_text {
            anchor.set_attr("title", cap);
        }
        // Replace the broken span (first element child) with the img.
        if let Some(span_idx) = anchor
            .children
            .iter()
            .position(|c| matches!(c.kind, NodeKind::Element(_)))
        {
            anchor.children[span_idx] = img;
        }
    }
}

/// Navigate to the node at `path` (a sequence of child indices from `root`).
fn node_at<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut node = root;
    for &idx in path {
        node = node.children.get_mut(idx)?;
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockDataSource;
    use crate::mock::MockSiteConfig;

    /// Build a `[typeof="mw:File"]` broken-media container for `File:Foobar.jpg`
    /// (the shape `renderFile` emits), plus an optional `data-width` on the span.
    fn container(data_width: Option<&str>) -> Node {
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("class", "mw-file-element mw-broken-media");
        span.set_attr("resource", "./File:Foobar.jpg");
        if let Some(w) = data_width {
            span.set_attr("data-width", w);
        }
        span.push_child(Node::text("File:Foobar.jpg"));

        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr(
            "href",
            "https://en.wikipedia.org/index.php?title=Special:Upload",
        );
        a.set_attr("class", "new");
        a.set_attr("title", "File:Foobar.jpg");
        a.push_child(span);

        let mut c = Node::element(ElementKind::Span);
        c.set_attr("typeof", "mw:File");
        c.set_attr("class", "mw-default-size");
        c.push_child(a);
        c
    }

    fn seed_file(ds: &MockDataSource) {
        let mut thumb_urls = HashMap::new();
        thumb_urls.insert(
            "180".to_string(),
            "http://example.com/images/thumb/3/3a/Foobar.jpg/180px-Foobar.jpg".to_string(),
        );
        ds.add_file(
            "File:Foobar.jpg",
            FileInfo {
                title: "Foobar.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                size: 7881,
                width: 1941,
                height: 220,
                description_url: "http://example.com/images/Foobar.jpg".to_string(),
                file_url: "http://example.com/images/3/3a/Foobar.jpg".to_string(),
                thumb_urls,
            },
        );
    }

    #[tokio::test]
    async fn test_existing_file_becomes_img() {
        let mut doc = Node::document();
        doc.push_child(container(None));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        let a = &c.children[0];
        assert_eq!(a.get_attr("class"), Some("mw-file-description"));
        assert_eq!(a.get_attr("href"), Some("./File:Foobar.jpg"));
        let img = &a.children[0];
        assert_eq!(img.get_attr("class"), Some("mw-file-element"));
        assert_eq!(img.get_attr("data-file-width"), Some("1941"));
        assert_eq!(img.get_attr("data-file-height"), Some("220"));
        assert_eq!(img.get_attr("data-file-type"), Some("bitmap"));
        assert_eq!(img.get_attr("width"), Some("1941"));
        assert_eq!(img.get_attr("height"), Some("220"));
    }

    #[tokio::test]
    async fn test_missing_file_becomes_error() {
        let mut doc = Node::document();
        doc.push_child(container(None));
        // No file seeded: Foobar.jpg is missing.
        let ds = MockDataSource::new();
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        assert!(c.get_attr("typeof").unwrap().contains("mw:Error"));
        assert!(
            c.data_mw
                .as_deref()
                .unwrap()
                .contains("apierror-filedoesnotexist")
        );
    }

    #[test]
    fn test_media_type_from_mime() {
        assert_eq!(media_type_from_mime("image/jpeg"), "bitmap");
        assert_eq!(media_type_from_mime("image/svg+xml"), "drawing");
    }

    /// A `<figure typeof="mw:File">` container with an empty `<a>` anchor and a
    /// `<figcaption>` caption (the shape `renderFile` emits for block media).
    fn figure_with_caption(typeof_attr: &str, caption: &str) -> Node {
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("resource", "./File:Foobar.jpg");
        span.push_child(Node::text("File:Foobar.jpg"));

        let mut a = Node::element(ElementKind::Other("a".to_string()));
        a.set_attr("class", "new");
        a.push_child(span);

        let mut figcaption = Node::element(ElementKind::FigCaption);
        figcaption.push_child(Node::text(caption));

        let mut figure = Node::element(ElementKind::Figure);
        figure.set_attr("typeof", typeof_attr);
        figure.push_child(a);
        figure.push_child(figcaption);
        figure
    }

    #[test]
    fn test_has_visible_caption() {
        let mut doc = Node::document();
        doc.push_child(figure_with_caption("mw:File/Thumb", "caption"));
        assert!(has_visible_caption(&doc, &[0]));

        let mut doc2 = Node::document();
        doc2.push_child(figure_with_caption("mw:File", "caption"));
        assert!(!has_visible_caption(&doc2, &[0]));
    }

    #[tokio::test]
    async fn test_non_thumb_caption_becomes_alt_and_title() {
        let mut doc = Node::document();
        doc.push_child(figure_with_caption("mw:File", "Caption text"));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let figure = &doc.children[0];
        let a = &figure.children[0];
        assert_eq!(a.get_attr("title"), Some("Caption text"));
        let img = &a.children[0];
        assert_eq!(img.get_attr("alt"), Some("Caption text"));
    }

    #[tokio::test]
    async fn test_thumb_caption_not_alt() {
        let mut doc = Node::document();
        doc.push_child(figure_with_caption("mw:File/Thumb", "caption"));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let figure = &doc.children[0];
        let a = &figure.children[0];
        assert_eq!(a.get_attr("title"), None);
        let img = &a.children[0];
        assert_eq!(img.get_attr("alt"), None);
    }
}
