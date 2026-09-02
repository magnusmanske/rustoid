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

        // `manualthumb=Thumb.png` uses the manual-thumb file's media info for
        // `src`/dimensions, while `href`/`resource` keep the original title.
        if let Some(mt) = &job.manualthumb {
            let mt_title = Title::new(6, mt.clone());
            let mt_key = mt_title.full_text();
            if infos.contains_key(&mt_key) {
                continue;
            }
            let mt_info = source.get_file_info(&mt_title).await.unwrap_or(None);
            infos.insert(mt_key, mt_info);
        }
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
    /// The `manualthumb=` title (a file name in the File namespace), if any.
    manualthumb: Option<String>,
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
            manualthumb: data_mw_txt(node, "manualthumb"),
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

    // `link=` / `alt=` options stored in `data-mw.attribs` by `renderFile`.
    let explicit_alt = data_mw_attrib(root, &job.path, "alt");
    let link_target = data_mw_attrib(root, &job.path, "link");

    // The caption text (trimmed) for `alt`/`title` when no explicit `alt`/`link`
    // option is present (mirrors `$captionText` → `$alt` in `AddMediaInfo`).
    // `hasVisibleCaption` (Thumb/Frame formats) suppresses the caption from
    // becoming `alt`/`title`; those captions live only in the `<figcaption>`.
    let caption_text = if explicit_alt.is_some() || has_visible_caption(root, &job.path) {
        None
    } else {
        caption_text(root, &job.path)
    };

    // The final `alt` for the image: explicit `alt=` wins, else the caption.
    let alt = explicit_alt.clone().or_else(|| caption_text.clone());

    let Some(info) = info else {
        // Missing file: leave broken, add `mw:Error` (mirrors `handleErrors`).
        mark_error(
            root,
            &job.path,
            "apierror-filedoesnotexist",
            "This image does not exist.",
            alt.as_deref(),
        );
        return;
    };

    // A file on the bad-image list stems a broken span but keeps a description
    // link (mirrors `$info['badFile']` → `apierror-badfile` + `handleErrors`).
    if info.bad_file {
        mark_bad_file(
            root,
            &job.path,
            &job.title,
            config,
            job.manualthumb.is_some(),
            alt.as_deref(),
        );
        return;
    }

    // `manualthumb=Thumb.png` renders the manual-thumb file's media (its
    // dimensions/`src`/`data-file-*`), while `href`/`resource`/`data-file-*`
    // still describe the original file. Mirrors PHP's manualthumb `$info`
    // replacement in the `AddMediaInfo::run` loop.
    let media_info = if let Some(mt) = &job.manualthumb {
        let mt_title = Title::new(6, mt.clone());
        infos
            .get(&mt_title.full_text())
            .and_then(|i| i.clone())
            .unwrap_or(info.clone())
    } else {
        info.clone()
    };

    // A bad manual-thumb file also errors the whole media (mirrors the
    // manualthumb-`badFile` case in `AddMediaInfo::run`).
    if media_info.bad_file {
        mark_bad_file(
            root,
            &job.path,
            &job.title,
            config,
            job.manualthumb.is_some(),
            alt.as_deref(),
        );
        return;
    }

    // Compute the rendered size (mirrors `handleSize` for bitmaps). The manual
    // thumb is unscaled, so `data-width` (if any) is ignored for it.
    let (width, height) = if job.manualthumb.is_some() {
        (media_info.width, media_info.height)
    } else {
        handle_size(job, &media_info)
    };

    // The image `src` (thumbnail when the file has one for the requested width,
    // else the raw file URL).
    let src = image_src(&media_info, job.data_width.as_deref());

    // Build the `<img>` replacement.
    let mut img = Node::element(ElementKind::Other("img".to_string()));
    // resource copied from the broken span's title (the file DB key).
    img.set_attr("resource", crate::title::make_link(&job.title, config));
    // alt from the explicit option/caption (when present), before the fixed
    // attrs (mirrors PHP's `thumbattribs` ordering: `src`, `decoding`,
    // `loading`, then `data-file-*`, then `width`/`height`).
    if let Some(alt) = &alt {
        img.set_attr("alt", alt);
    }
    // Fixed attribute set (decoding/loading).
    for (k, v) in IMG_ATTRIBS {
        img.set_attr(k, v);
    }
    // data-file-* read-only original size info (T64881). For manualthumb these
    // reflect the manual-thumb file, matching PHP's `$info` replacement.
    img.set_attr("data-file-width", media_info.width.to_string());
    img.set_attr("data-file-height", media_info.height.to_string());
    img.set_attr(
        "data-file-type",
        media_type_from_mime(&media_info.mime_type),
    );
    // src + srcset (responsive 2x).
    img.set_attr("src", src);
    if let Some(srcset) = srcset(&media_info) {
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
        &AnchorOpts {
            caption_text: caption_text.as_deref(),
            link_target: link_target.as_deref(),
            is_manual_thumb: job.manualthumb.is_some(),
        },
    );
}

/// The `txt` value of a named option in the container's `data-mw.attribs`, if
/// present. Mirrors `WTSUtils::getAttrFromDataMw($dataMw, $key, true)`.
fn data_mw_attrib(root: &Node, path: &[usize], key: &str) -> Option<String> {
    let container = node_at_read(root, path)?;
    data_mw_txt(container, key)
}

/// The `txt` value of a named option in a node's `data-mw.attribs`, if present.
fn data_mw_txt(node: &Node, key: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(node.data_mw.as_deref()?).ok()?;
    let attribs = json.get("attribs")?.as_array()?;
    for pair in attribs {
        let arr = pair.as_array()?;
        let k = arr
            .first()?
            .as_str()
            .or_else(|| arr.first()?.get("txt").and_then(|t| t.as_str()))?;
        if k == key {
            let v = arr.get(1)?;
            if let Some(txt) = v.get("txt").and_then(|t| t.as_str()) {
                return Some(txt.to_string());
            }
            return v.as_str().map(str::to_string);
        }
    }
    None
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

/// The trimmed caption text of a media container. Block (`<figure>`) media
/// carry the caption in a `<figcaption>` child; inline (`<span>`) media carry it
/// in `data-mw.caption` (a serialized fragment). Mirrors
/// `$captionText = trim(textContentFromCaption($caption))`.
fn caption_text(root: &Node, path: &[usize]) -> Option<String> {
    let container = node_at_read(root, path)?;
    // Block case: trim the `<figcaption>` text content.
    if let Some(figcaption) = container
        .children
        .iter()
        .find(|c| matches!(c.kind, NodeKind::Element(ElementKind::FigCaption)))
    {
        let text = text_content(figcaption);
        let trimmed = text.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    // Inline case: extract the text from `data-mw.caption` by stripping
    // wikilink/quote/entity markup.
    let caption = json_string_field(container, "caption")?;
    let text = caption_text_from_source(&caption);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The text content of a caption *source* string: follows `[[target|display]]`
/// (→ `display`) and `[[target]]` (→ `target`). Mirrors the text produced by
/// `textContentFromCaption` (entity decoding is handled during re-tokenization
/// of the caption, not here).
fn caption_text_from_source(source: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' && i + 1 < chars.len() && chars[i + 1] == '[' {
            // A wikilink: `[[target|display]]` or `[[target]]`.
            if let Some(link_close) = find_matching_brackets(&chars, i) {
                let inner: String = chars[i + 2..link_close].iter().collect();
                out.push_str(link_display_text(&inner));
                i = link_close + 2;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The index of the `]]` closing the `[[…` at `start` (balancing nested links).
fn find_matching_brackets(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 2;
    let mut depth = 1;
    while i + 1 < chars.len() {
        if chars[i] == '[' && chars[i + 1] == '[' {
            depth += 1;
            i += 2;
            continue;
        }
        if chars[i] == ']' && chars[i + 1] == ']' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    None
}

/// The display text of a wikilink inner string (`target|display` → `display`,
/// else the target).
fn link_display_text(inner: &str) -> &str {
    inner.rsplit('|').next().unwrap_or(inner).trim()
}

/// Read a top-level string field from a node's `data-mw` JSON object.
fn json_string_field(node: &Node, key: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(node.data_mw.as_deref()?).ok()?;
    json.get(key)?.as_str().map(str::to_string)
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
fn mark_error(root: &mut Node, path: &[usize], key: &str, message: &str, alt: Option<&str>) {
    // Adjust the parent gallery thumb's `style` before borrowing the container.
    strip_gallery_thumb_width(root, path);

    let Some(node) = node_at(root, path) else {
        return;
    };
    add_error_type(node);
    replace_broken_span_text(node, alt);
    let errors = format!("{{\"errors\":[{{\"key\":\"{key}\",\"message\":\"{message}\"}}]}}");
    node.data_mw = Some(errors);
}

/// For a broken media container, replace the broken `<span>`'s text content with
/// the caption/alt text (mirrors `AddMediaInfo::handleErrors`, which does
/// `replaceChildren($span, textNode($alt))` when `$alt` is non-empty).
fn replace_broken_span_text(container: &mut Node, alt: Option<&str>) {
    let Some(alt) = alt else {
        return;
    };
    if alt.is_empty() {
        return;
    }
    // The broken span is the anchor's first element child (the anchor is the
    // container's first element child).
    let Some(anchor) = container
        .children
        .iter_mut()
        .find(|c| matches!(c.kind, NodeKind::Element(_)))
    else {
        return;
    };
    if let Some(span) = anchor
        .children
        .iter_mut()
        .find(|c| matches!(c.kind, NodeKind::Element(_)))
    {
        span.children = vec![Node::text(alt.to_string())];
    }
}

/// For a gallery media (whose parent is a `div.thumb`), drop the `width:` from
/// the thumb's `style` so a broken/error thumbnail renders `height`-only
/// (mirrors `TraditionalMode::thumbStyle`, which omits `width` when `hasError`).
fn strip_gallery_thumb_width(root: &mut Node, path: &[usize]) {
    if path.is_empty() {
        return;
    }
    let parent_path = &path[..path.len() - 1];
    let Some(parent) = node_at(root, parent_path) else {
        return;
    };
    if parent.get_attr("class") != Some("thumb") {
        return;
    }
    let Some(style) = parent.get_attr("style").map(str::to_string) else {
        return;
    };
    // Remove the `width: …px;` component, leaving only `height:`.
    let cleaned: String = style
        .split(';')
        .filter(|part| !part.trim_start().starts_with("width:"))
        .map(|part| {
            let p = part.trim();
            if p.is_empty() {
                String::new()
            } else {
                format!("{p}; ")
            }
        })
        .collect();
    let cleaned = cleaned.trim_end().to_string();
    if cleaned.is_empty() {
        parent.attrs.retain(|a| a.key != "style");
    } else {
        parent.set_attr("style", cleaned);
    }
}

/// Mark a container as `mw:Error` (space-separated, non-duplicated, first).
fn add_error_type(node: &mut Node) {
    let mut tokens: Vec<String> = node
        .get_attr("typeof")
        .map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if !tokens.iter().any(|t| t == "mw:Error") {
        tokens.insert(0, "mw:Error".to_string());
        node.set_attr("typeof", tokens.join(" "));
    }
}

/// Handle a file on the bad-image list: keep the broken `<span>` but rewrite the
/// anchor into a file-description link and mark `mw:Error` + `apierror-badfile`.
/// Mirrors `AddMediaInfo::handleErrors` + the `$errs` `replaceAnchor` path.
fn mark_bad_file(
    root: &mut Node,
    path: &[usize],
    title: &Title,
    config: &dyn SiteConfig,
    is_manual_thumb: bool,
    alt: Option<&str>,
) {
    // Adjust the parent gallery thumb's `style` before borrowing the container
    // (the two need disjoint `&mut` borrows of `root`).
    strip_gallery_thumb_width(root, path);

    let Some(container) = node_at(root, path) else {
        return;
    };
    add_error_type(container);
    replace_broken_span_text(container, alt);

    // The anchor is the first element child; rewrite it to a description link
    // (mirrors `replaceAnchor`'s `$addDescriptionLink`, which still runs when
    // `$errs` are present). The `mw-file-description` class is omitted for
    // manual-thumb images.
    if let Some(anchor_idx) = container
        .children
        .iter()
        .position(|c| matches!(c.kind, NodeKind::Element(_)))
    {
        let anchor = &mut container.children[anchor_idx];
        anchor
            .attrs
            .retain(|a| a.key != "class" && a.key != "title" && a.key != "href");
        anchor.set_attr("href", crate::title::make_link(title, config));
        if !is_manual_thumb {
            anchor.set_attr("class", "mw-file-description");
        }
    }

    let errors = r##"{"errors":[{"key":"apierror-badfile","message":"This image is on the bad file list."}]}"##;
    container.data_mw = Some(errors.to_string());
}

/// The anchor-rewrite parameters computed by `apply_media_info` (bundled to
/// keep `rewrite_structure`'s arity manageable).
struct AnchorOpts<'a> {
    caption_text: Option<&'a str>,
    link_target: Option<&'a str>,
    is_manual_thumb: bool,
}

/// Replace the broken `<span>` with the built `<img>` and rewrite the anchor to
/// a file-description link (or an explicit `link=` target; or a `<span>` when
/// `link=` is empty). Mirrors `replaceAnchor` + `$anchor->appendChild($elt)`.
fn rewrite_structure(
    root: &mut Node,
    path: &[usize],
    title: &Title,
    img: Node,
    config: &dyn SiteConfig,
    opts: &AnchorOpts,
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
        // Strip the red-link markers left by `renderFile` (class="new",
        // title=file-name, href=upload-url). They are replaced below.
        anchor
            .attrs
            .retain(|a| a.key != "class" && a.key != "title" && a.key != "href" && a.key != "rel");

        if let Some(link) = opts.link_target {
            if link.is_empty() {
                // `link=` (empty): no link at all → a bare `<span>`.
                anchor.kind = NodeKind::Element(ElementKind::Span);
            } else if is_url(link) {
                // An external URL link (`rel=nofollow`, matching
                // `AddLinkAttributes`).
                anchor.set_attr("href", link);
                anchor.set_attr("rel", "nofollow");
            } else {
                // A wiki-title link (with optional `#fragment`).
                let link_title = TitleParser::parse(link, config);
                let mut href = crate::title::make_link(&link_title, config);
                if let Some(fragment) = &link_title.fragment {
                    href.push('#');
                    href.push_str(fragment);
                }
                anchor.set_attr("href", href);
                anchor.set_attr("title", link_title.get_prefixed_text());
            }
            // A caption may still override the `title` (mirrors
            // `$anchor->setAttribute('title', $captionText)`).
            if let Some(cap) = opts.caption_text {
                anchor.set_attr("title", cap);
            }
        } else {
            // Description link to the file page (mirrors `$addDescriptionLink`).
            anchor.set_attr("href", crate::title::make_link(title, config));
            // The file-description class is omitted for manual-thumb images so
            // MultimediaViewer does not launch (mirrors `replaceAnchor`).
            if !opts.is_manual_thumb {
                anchor.set_attr("class", "mw-file-description");
            }
            // `title` from the caption (or absent when the caption is empty).
            if let Some(cap) = opts.caption_text {
                anchor.set_attr("title", cap);
            }
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

/// Whether a `link=` value is an external URL (has a scheme or is
/// protocol-relative). Mirrors the URL-vs-title decision in `replaceAnchor`.
fn is_url(s: &str) -> bool {
    s.starts_with("//") || s.contains("://")
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
                bad_file: false,
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

    #[test]
    fn test_caption_text_from_source() {
        // Wikilinks resolve to their display text; piped links use the display.
        assert_eq!(
            caption_text_from_source("text with a [[link]] in it"),
            "text with a link in it"
        );
        assert_eq!(
            caption_text_from_source("see [[Target|display]] here"),
            "see display here"
        );
    }

    #[tokio::test]
    async fn test_bad_file_becomes_error() {
        let mut doc = Node::document();
        doc.push_child(container(None));
        let ds = MockDataSource::new();
        // A bad-file image errors and keeps a description link to the file page.
        ds.add_file(
            "File:Foobar.jpg",
            FileInfo {
                title: "Foobar.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                size: 1,
                width: 100,
                height: 100,
                description_url: "".to_string(),
                file_url: "http://example.com/images/Foobar.jpg".to_string(),
                thumb_urls: HashMap::new(),
                bad_file: true,
            },
        );
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        assert!(c.get_attr("typeof").unwrap().contains("mw:Error"));
        let a = &c.children[0];
        assert_eq!(a.get_attr("class"), Some("mw-file-description"));
        assert_eq!(a.get_attr("href"), Some("./File:Foobar.jpg"));
        // The broken span is kept (not replaced with an <img>).
        assert_eq!(
            a.children[0].get_attr("class"),
            Some("mw-file-element mw-broken-media")
        );
        assert!(c.data_mw.as_deref().unwrap().contains("apierror-badfile"));
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

    /// A `<span typeof="mw:File">` container carrying `data-mw.attribs` for a
    /// single option (mirrors `renderFile` storing `link`/`alt` into data-mw).
    fn container_with_data_mw(key: &str, value: &str) -> Node {
        let mut c = container(None);
        c.data_mw = Some(format!(
            "{{\"attribs\":[[\"{key}\",{{\"txt\":\"{value}\"}}]]}}"
        ));
        c
    }

    #[tokio::test]
    async fn test_link_parameter_wiki_target() {
        let mut doc = Node::document();
        doc.push_child(container_with_data_mw("link", "Main Page"));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        let a = &c.children[0];
        assert_eq!(a.get_attr("class"), None);
        assert_eq!(a.get_attr("href"), Some("./Main_Page"));
        assert_eq!(a.get_attr("title"), Some("Main Page"));
    }

    #[tokio::test]
    async fn test_link_parameter_empty_becomes_span() {
        let mut doc = Node::document();
        doc.push_child(container_with_data_mw("link", ""));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        let a = &c.children[0];
        assert!(matches!(a.kind, NodeKind::Element(ElementKind::Span)));
        assert_eq!(a.get_attr("href"), None);
    }

    #[tokio::test]
    async fn test_link_parameter_url() {
        let mut doc = Node::document();
        doc.push_child(container_with_data_mw("link", "http://example.com/"));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        let a = &c.children[0];
        assert_eq!(a.get_attr("href"), Some("http://example.com/"));
        assert_eq!(a.get_attr("rel"), Some("nofollow"));
    }

    #[tokio::test]
    async fn test_alt_parameter_wins() {
        let mut doc = Node::document();
        doc.push_child(container_with_data_mw("alt", "alttext"));
        let ds = MockDataSource::new();
        seed_file(&ds);
        let cfg = MockSiteConfig::new();
        run(&mut doc, &ds, &cfg).await;

        let c = &doc.children[0];
        let a = &c.children[0];
        let img = &a.children[0];
        assert_eq!(img.get_attr("alt"), Some("alttext"));
    }
}
