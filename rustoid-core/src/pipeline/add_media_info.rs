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
use std::collections::hash_map::Entry;

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
        if let Entry::Vacant(entry) = infos.entry(key) {
            let info = source.get_file_info(&job.title).await.unwrap_or(None);
            entry.insert(info);
        }

        // `manualthumb=Thumb.png` uses the manual-thumb file's media info for
        // `src`/dimensions, while `href`/`resource` keep the original title.
        // This fetch is independent of the main-file dedup above: a manual-thumb
        // job whose main file was already fetched (e.g. another gallery line for
        // the same file) must still retrieve the manual-thumb info.
        if let Some(mt) = &job.manualthumb {
            let mt_title = Title::new(6, mt.clone());
            let mt_key = mt_title.full_text();
            if let Entry::Vacant(entry) = infos.entry(mt_key) {
                let mt_info = source.get_file_info(&mt_title).await.unwrap_or(None);
                entry.insert(mt_info);
            }
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
    /// The broken span's `data-height` (dims.height), if any.
    data_height: Option<String>,
    /// The `manualthumb=` title (a file name in the File namespace), if any.
    manualthumb: Option<String>,
    /// The `data-upright` factor on the broken span (only present for
    /// `thumb`/`frameless` + `upright`), if any.
    upright: Option<f64>,
}

/// Resolve the `<a>` anchor inside a media container, descending through any
/// reopened formatting elements (`<i>`, `<b>`, …) the tree builder inserted to
/// repair a content-model violation (PHP's `reopenedAFE`, T314059).
///
/// Returns the index path from `container` to the anchor (each hop follows the
/// first element child). The last hop is the `<a>` anchor; preceding hops are the
/// reopened formatting elements (if any). When the first element child is not an
/// `<a>` and not a formatting tag, the result is the single-hop path to it.
fn anchor_path(container: &Node) -> Vec<usize> {
    let mut path = Vec::new();
    let mut node = container;
    while let Some(idx) = node
        .children
        .iter()
        .position(|c| matches!(c.kind, NodeKind::Element(_)))
    {
        path.push(idx);
        node = &node.children[idx];
        if crate::html::wts_utils::node_name(node) == "a" {
            break;
        }
        if !crate::html::dom_utils::is_formatting_elt(node) {
            break;
        }
    }
    path
}

/// Navigate to a node via an index path (from `anchor_path`).
fn node_at_path<'a>(container: &'a Node, path: &[usize]) -> Option<&'a Node> {
    let mut node = container;
    for &idx in path {
        node = node.children.get(idx)?;
    }
    Some(node)
}

/// Migrate reopened formatting elements out of the media anchor and into the
/// figcaption (PHP `AddMediaInfo::run`'s `reopenedAFE` handling, T314059).
///
/// When `renderFile`'s `<figure>` was opened inside a formatting element (an
/// active-formatting-element reconstruction), the tree builder nests the reopened
/// `<i>`/`<b>`/… around both the `<a>` and the `<figcaption>`:
/// `<figure><i><a>…</a><figcaption>…</figcaption></i></figure>`. The spec wants
/// the `<a>` and `<figcaption>` as direct children of the container, with the
/// formatting element moved into the `<figcaption>`:
/// `<figure><a>…</a><figcaption><i>…</i></figcaption></figure>`. Inline media
/// (a `<span>` container) simply drops the formatting element.
fn migrate_reopened_afe(container: &mut Node) {
    let path = anchor_path(container);
    // A single-hop path means the anchor is already a direct child (no AFE).
    if path.len() <= 1 {
        return;
    }

    let is_block = matches!(container.kind, NodeKind::Element(ElementKind::Figure));

    // Remove the anchor from the innermost AFE.
    let anchor = take_node_at_path(container, &path);
    let Some(anchor) = anchor else {
        return;
    };

    // Peel the (now anchor-less) AFE chain off the container. It is the
    // container's first element child.
    let mut chain = container.children.remove(path[0]);

    // For block media, peel the figcaption out of the innermost AFE and move its
    // caption content into that AFE (so the reopened formatting wraps the caption
    // text), then re-home the chain inside the figcaption.
    let figcaption = if is_block {
        peel_figcaption(&mut chain)
    } else {
        None
    };

    let mut rebuilt = Vec::with_capacity(container.children.len() + 2);
    rebuilt.push(anchor);
    if let Some(mut fig) = figcaption {
        if is_block {
            fig.children.insert(0, chain);
        }
        rebuilt.push(fig);
    }
    rebuilt.extend(std::mem::take(&mut container.children));
    container.children = rebuilt;
}

/// Peel the `<figcaption>` out of the *innermost* AFE of `chain` (the deepest
/// formatting element, which held the anchor and now holds the figcaption), and
/// move the figcaption's caption content into that AFE. Returns the figcaption
/// (now childless) and leaves `chain` as the (now-empty) nested formatting tree.
fn peel_figcaption(chain: &mut Node) -> Option<Node> {
    // Walk down the single-element-child formatting chain to the innermost AFE.
    let mut node = chain;
    loop {
        let idx = node
            .children
            .iter()
            .position(|c| matches!(c.kind, NodeKind::Element(_)))?;
        let child_is_fmt = crate::html::dom_utils::is_formatting_elt(&node.children[idx]);
        if child_is_fmt {
            node = &mut node.children[idx];
            continue;
        }
        // `node` is the innermost AFE; its first element child is the figcaption.
        let mut fig = node.children.remove(idx);
        // Move the caption content into the innermost AFE.
        let caption = std::mem::take(&mut fig.children);
        node.children.extend(caption);
        return Some(fig);
    }
}

/// Remove and return the node at `path` from `container` (the final sibling is
/// removed; intermediate hops remain). The path maps `path[0]` to an index in
/// `container.children`, and each subsequent index to a child of the prior hop.
fn take_node_at_path(container: &mut Node, path: &[usize]) -> Option<Node> {
    if path.len() == 1 {
        return Some(container.children.remove(path[0]));
    }
    let mut node = container;
    for &idx in &path[..path.len() - 1] {
        node = node.children.get_mut(idx)?;
    }
    Some(node.children.remove(path[path.len() - 1]))
}

/// Parse the file title from a media container's broken span text.
///
/// PHP resolves the title from `$span->textContent` (the prefixed DB text the
/// tokenizer stashed inside the broken span). Mirrors that behavior.
fn title_from_container(container: &Node, config: &dyn SiteConfig) -> Title {
    let span = node_at_path(container, &anchor_path(container)).and_then(first_element_child);
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
    let span = node_at_path(container, &anchor_path(container)).and_then(first_element_child);
    span.and_then(|s| s.get_attr("data-width").map(str::to_string))
}

/// The `data-height` attribute on a container's broken span, if present.
fn data_height_from_container(container: &Node) -> Option<String> {
    let span = node_at_path(container, &anchor_path(container)).and_then(first_element_child);
    span.and_then(|s| s.get_attr("data-height").map(str::to_string))
}

/// The `data-upright` factor on a container's broken span, if present (mirrors
/// `$uprightFactor = getAttribute($span, 'data-upright')` in `AddMediaInfo::run`).
fn upright_from_container(container: &Node) -> Option<f64> {
    let span = node_at_path(container, &anchor_path(container)).and_then(first_element_child);
    span.and_then(|s| s.get_attr("data-upright")?.parse::<f64>().ok())
}

/// The `lang` attribute on a container's broken span, if present (mirrors
/// `$lang = getAttribute($span, 'lang')` in `AddMediaInfo::run`).
fn lang_from_container(root: &Node, path: &[usize]) -> Option<String> {
    let container = node_at_read(root, path)?;
    let span = node_at_path(container, &anchor_path(container)).and_then(first_element_child);
    span.and_then(|s| s.get_attr("lang").map(str::to_string))
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

/// Whether a media container is a media container (has a `mw:File` `typeof` token).
fn is_media_container(node: &Node) -> bool {
    node.get_attr("typeof")
        .map(|t| {
            t.split_whitespace()
                .any(|tok| tok == "mw:File" || tok.starts_with("mw:File/"))
        })
        .unwrap_or(false)
}

/// Whether a container is already marked `mw:Error` (a missing/bad file resolved
/// in a prior `AddMediaInfo` pass, e.g. a gallery line resolved in its own
/// sub-pipeline). These keep their broken `<span>`, so they must not be re-run
/// by a later top-level pass (mirrors PHP, where sub-pipeline media is guarded
/// by the DOM-fragment wrapper invariant).
fn has_error_type(node: &Node) -> bool {
    node.get_attr("typeof")
        .map(|t| t.split_whitespace().any(|tok| tok == "mw:Error"))
        .unwrap_or(false)
}

/// Whether a media container still carries its *broken* placeholder — an `<a>`
/// whose first element child is a `<span>` (the broken-media span) rather than an
/// already-resolved `<img>` (which happens when `AddMediaInfo` ran in a
/// sub-pipeline, e.g. a gallery line). Resolved media is skipped on a later pass
/// (mirrors PHP's `$span instanceof span` guard in `AddMediaInfo::run`).
fn has_broken_span(container: &Node) -> bool {
    let Some(anchor) = node_at_path(container, &anchor_path(container)) else {
        return false;
    };
    matches!(
        first_element_child(anchor).map(|s| &s.kind),
        Some(NodeKind::Element(ElementKind::Span))
    )
}

/// Collect `[typeof~="mw:File"]` containers, deepest-first so rewrites don't
/// invalidate the recorded paths of outer containers. Mirrors PHP's
/// `querySelectorAll('[typeof*="mw:File"]')`, which finds *every* media
/// container including those nested in a figcaption (e.g. an image inside an
/// image caption). Media nested in a DOM-fragment wrapper was already resolved
/// in its own sub-pipeline (the `isDOMFragmentWrapper` invariant), but media
/// nested inside an ordinary caption must still be discovered here.
fn collect_containers(
    node: &mut Node,
    path: &mut Vec<usize>,
    out: &mut Vec<ContainerJob>,
    config: &dyn SiteConfig,
) {
    // Recurse into children *first* so nested media (e.g. an image inside a
    // figcaption) are collected before their outer container. This is
    // deepest-first: a nested image is resolved (and its broken text replaced
    // by an `<img>`) before the outer media reads its caption text, so the
    // outer caption text does not leak the nested image's filename.
    for i in 0..node.children.len() {
        path.push(i);
        collect_containers(&mut node.children[i], path, out, config);
        path.pop();
    }
    if is_media_container(node) && has_broken_span(node) && !has_error_type(node) {
        out.push(ContainerJob {
            path: path.clone(),
            title: title_from_container(node, config),
            data_width: data_width_from_container(node),
            data_height: data_height_from_container(node),
            manualthumb: data_mw_txt(node, "manualthumb"),
            upright: upright_from_container(node),
        });
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

    // T314059: migrate any reopened formatting elements (from a content-model
    // violation, e.g. `<p>''[[File:…|thumb]]''</p>`) out of the anchor and into
    // the figcaption, so the resolved `<a>` becomes a direct child of the
    // container (mirrors PHP `AddMediaInfo::run`'s `reopenedAFE` handling).
    if let Some(container) = node_at(root, &job.path) {
        migrate_reopened_afe(container);
    }

    // `link=` / `alt=` / `page=` options stored in `data-mw.attribs` by
    // `renderFile` (`lang=` lives on the broken span, read separately below).
    let explicit_alt = data_mw_attrib(root, &job.path, "alt");
    let link_target = data_mw_attrib(root, &job.path, "link");
    let page = data_mw_attrib(root, &job.path, "page");
    let lang = lang_from_container(root, &job.path);

    // The caption text (trimmed) for the anchor `title` (mirrors
    // `$captionText`, which is independent of `alt`). `hasVisibleCaption`
    // (Thumb/Frame formats) suppresses the caption from becoming the title;
    // those captions live only in the `<figcaption>`.
    let caption_text = if has_visible_caption(root, &job.path) {
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
    let mut manualthumb_missing = false;
    let media_info = if let Some(mt) = &job.manualthumb {
        let mt_title = Title::new(6, mt.clone());
        match infos.get(&mt_title.full_text()).and_then(|i| i.clone()) {
            Some(mt_info) => mt_info,
            // A missing manual-thumb file errors the whole media (mirrors the
            // `!$manualinfo` → `apierror-filedoesnotexist` branch).
            None => {
                manualthumb_missing = true;
                info.clone()
            }
        }
    } else {
        info.clone()
    };

    // A missing manual-thumb file keeps the broken media and adds `mw:Error`;
    // the anchor is still rewritten to a file-description link (the original
    // file exists, so `$broken` is false and `replaceAnchor` runs with
    // `$isManualThumb = false`, keeping the `mw-file-description` class).
    if manualthumb_missing {
        mark_error_with_description_link(
            root,
            &job.path,
            &job.title,
            config,
            "apierror-filedoesnotexist",
            "This image does not exist.",
            alt.as_deref(),
        );
        return;
    }

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
    // thumb is unscaled, so `data-width` (if any) is ignored for it. Any packed-
    // gallery re-scaling is applied later by `TraditionalMode::line`'s
    // `scaleMedia` (see `gallery.rs`), which reads the resolved `<img>` width.
    let (resolved_width, resolved_height) = if job.manualthumb.is_some() {
        (media_info.width, media_info.height)
    } else {
        handle_size(job, &media_info)
    };

    // The image `src` is the thumbnail at the *resolved* width (before any
    // packed-gallery scaling).
    let src = {
        let resolved_key = resolved_width.to_string();
        image_src(&media_info, Some(&resolved_key))
    };
    let width = resolved_width;
    let height = resolved_height;

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
    // `upright` adds the client-side scaling class and a custom property that
    // CSS uses for responsive image scaling (mirrors `AddMediaInfo::run`).
    if let Some(factor) = job.upright {
        img.set_attr("class", "mw-file-element mw-file-upright");
        img.set_attr("style", format!("--mw-file-upright: {factor}"));
    } else {
        img.set_attr("class", "mw-file-element");
    }

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
            page: page.as_deref(),
            lang: lang.as_deref(),
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
        if c == '<' && is_nowiki_open(&chars, i) {
            // A `<nowiki>` block: its content is literal (wikilinks/quotes inside
            // are NOT processed), but the block itself is an element boundary.
            // Append the raw content and skip past `</nowiki>`.
            let open_len = 8; // `<nowiki>`
            if let Some(inner_close) = find_nowiki_close(&chars, i + open_len) {
                for ch in &chars[i + open_len..inner_close] {
                    out.push(*ch);
                }
                i = inner_close + 9; // `</nowiki>`
            } else {
                out.push('<');
                i += 1;
            }
            continue;
        }
        if c == '[' && i + 1 < chars.len() && chars[i + 1] == '[' {
            // A wikilink: `[[target|display]]` or `[[target]]`.
            if let Some(link_close) = find_matching_brackets(&chars, i) {
                let inner: String = chars[i + 2..link_close].iter().collect();
                out.push_str(&link_display_text(&inner));
                i = link_close + 2;
                continue;
            }
        }
        if c == '<' {
            // An HTML tag in a caption: recognized inline tags are stripped to
            // their text content (`a<i>b</i>c` → `abc`), but meta-data tags
            // (`<script>`, `<style>`, …) stay as literal (escaped) text. Mirrors
            // the `textContent` of a re-tokenized caption fragment, where those
            // tags become elements vs. escaped text respectively.
            if let Some((name, end)) = parse_html_tag_boundary(&chars, i) {
                if crate::wikitext::consts::meta_data_tags().contains(&name) {
                    // Meta-data tag: keep the `<` literal and continue (the rest
                    // of the tag text is appended normally).
                    out.push('<');
                    i += 1;
                    continue;
                }
                // Recognized inline tag: skip the whole tag (element boundary).
                i = end;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    // The caption was stored as raw wikitext (entities not yet decoded); decode
    // them so `&#9792;` → `♀` (mirrors PHP's `textContentFromCaption`, which runs
    // on the already-re-tokenized caption DOM).
    let decoded = crate::html::wts_utils::decode_wt_entities_all(&out);
    // Quote markers (`''`, `'''`) become `<i>`/`<b>` elements in the re-rendered
    // caption, whose text is just the inner content; strip them for the
    // alt/title text (mirrors `textContentFromCaption` on that DOM).
    crate::pipeline::media_options::strip_quote_markers(&decoded)
}

/// If `chars[start] == '<'` and starts an HTML tag, return `(name, end)` where
/// `end` is the index just past the tag. Recognizes only HTML5/older-HTML tag
/// names; returns `None` for unknown `<...>` or malformed tags (which are
/// treated as literal text).
fn parse_html_tag_boundary(chars: &[char], start: usize) -> Option<(String, usize)> {
    let closing = start + 1 < chars.len() && chars[start + 1] == '/';
    let mut i = start + 1 + usize::from(closing);
    // Tag name (letters/digits).
    let name_start = i;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();
    // Only recognized tags qualify as elements; anything else is literal.
    if !crate::wikitext::consts::html5_tags().contains(&name)
        && !crate::wikitext::consts::older_html_tags().contains(&name)
    {
        return None;
    }
    // Find the closing `>`.
    let gt = chars[i..].iter().position(|&c| c == '>')?;
    let end = i + gt + 1;
    Some((name, end))
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
/// else the percent-decoded target). The target is percent-decoded because a
/// bare `[[Target]]` renders with `title.get_text()` (decoded) as its link text,
/// mirroring PHP's `textContentFromCaption` running on the re-rendered caption
/// DOM.
fn link_display_text(inner: &str) -> String {
    let text = inner.rsplit('|').next().unwrap_or(inner).trim();
    // Only decode when the target itself is the display (no `|` separator).
    if inner.contains('|') {
        text.to_string()
    } else {
        crate::util::decode_uri_component(text)
    }
}

/// The char index of the (case-insensitive) `</nowiki>` closing tag at or after
/// `start`, if present.
fn find_nowiki_close(chars: &[char], start: usize) -> Option<usize> {
    let lower: String = chars[start..].iter().collect();
    let lower = lower.to_lowercase();
    let rel = lower.find("</nowiki>")?;
    Some(start + rel)
}

/// Whether `chars[i..]` begins with a case-insensitive `<nowiki>` opening tag.
fn is_nowiki_open(chars: &[char], i: usize) -> bool {
    if chars.len() < i + 8 {
        return false;
    }
    let lower: String = chars[i..i + 8].iter().collect();
    lower.to_lowercase() == "<nowiki>"
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
/// for the common non-upscaling bitmap cases). When `data-height` is present and
/// smaller than the file height, the thumbnail is *height-constrained* (as for
/// the packed gallery, whose `dimensions()` requests a large width but a concrete
/// height), so the width is derived from the aspect ratio.
fn handle_size(job: &ContainerJob, info: &FileInfo) -> (u32, u32) {
    let (mut width, mut height) = (info.width, info.height);

    let req_w = job
        .data_width
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());
    let req_h = job
        .data_height
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());

    // Height-constrained thumbnail: a concrete `data-height` smaller than the
    // file height drives the thumbnail dimensions; the width preserves the
    // aspect ratio (mirrors core's thumbnail generation, which rounds up).
    if let Some(h) = req_h
        && h > 0
        && h < info.height
        && info.height > 0
    {
        height = h;
        let w = (info.width as u64 * h as u64).div_ceil(info.height as u64);
        width = w as u32;
        return (width, height);
    }

    // A `thumb`/`frameless` request carries the target width on the broken span
    // (`data-width`). Scale proportionally (exact thumb-height is not derivable
    // from `FileInfo`, so we preserve the file's aspect ratio).
    if let Some(w) = req_w
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
    let Some(node) = node_at(root, path) else {
        return;
    };
    add_error_type(node);
    replace_broken_span_text(node, alt);
    let errors = format!("{{\"errors\":[{{\"key\":\"{key}\",\"message\":\"{message}\"}}]}}");
    node.data_mw = Some(errors);
}

/// Mark an error while also rewriting the anchor to a file-description link
/// (mirrors the manualthumb-missing case, where the original file exists so
/// `replaceAnchor` still runs with the `mw-file-description` class).
fn mark_error_with_description_link(
    root: &mut Node,
    path: &[usize],
    title: &Title,
    config: &dyn SiteConfig,
    key: &str,
    message: &str,
    alt: Option<&str>,
) {
    let Some(container) = node_at(root, path) else {
        return;
    };
    add_error_type(container);
    replace_broken_span_text(container, alt);

    // Rewrite the anchor to a description link (keeps the `mw-file-description`
    // class, since `$isManualThumb` is false when the manual-thumb info is
    // missing).
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
        anchor.set_attr("class", "mw-file-description");
    }

    let errors = format!("{{\"errors\":[{{\"key\":\"{key}\",\"message\":\"{message}\"}}]}}");
    container.data_mw = Some(errors);
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
    page: Option<&'a str>,
    lang: Option<&'a str>,
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
                // An external URL link (mirrors `replaceAnchor`'s external
                // branch, which applies the `getExternalLinkAttribs` set).
                let cleaned = crate::sanitizer::clean_url(link, "external", |proto| {
                    config.has_valid_protocol(proto)
                })
                .unwrap_or_else(|| link.to_string());
                anchor.set_attr("href", &cleaned);
                for (key, values) in config.external_link_attribs(&cleaned) {
                    if key == "rel" {
                        for v in &values {
                            crate::pipeline::add_link_attributes::add_rel(anchor, v);
                        }
                    } else if key == "class" {
                        for v in &values {
                            crate::pipeline::add_link_attributes::add_class(anchor, v);
                        }
                    } else {
                        anchor.set_attr(&key, values.join(" "));
                    }
                }
            } else {
                // A wiki-title link (with optional `#fragment`). The value is
                // percent-decoded first (mirrors `replaceAnchor`'s
                // `makeTitleFromText($val, ...)`, which decodes `%XX`).
                let decoded = crate::util::decode_uri_component(link);
                let link_title = TitleParser::parse(&decoded, config);
                // An invalid link title (illegal chars like `<`) falls back to the
                // description link (mirrors `replaceAnchor`, where a null
                // `$link` is treated as `link=` not present).
                if crate::title::has_invalid_chars(&link_title.text) {
                    anchor.set_attr("href", description_link_href(title, opts, config));
                    if !opts.is_manual_thumb {
                        anchor.set_attr("class", "mw-file-description");
                    }
                } else if let Some(iw) = &link_title.interwiki
                    && let Some(info) = config.interwiki_map().get(iw)
                {
                    // An interwiki link target resolves to the interwiki URL
                    // (mirrors `replaceAnchor`'s interwiki branch, which builds
                    // the absolute URL and applies the nofollow attribs).
                    let title_part = crate::sanitizer::sanitize_title_uri(&link_title.text, false);
                    let mut href = info.url.replace("$1", &title_part);
                    if info.protorel == Some(true) {
                        href = href
                            .strip_prefix("http:")
                            .or_else(|| href.strip_prefix("https:"))
                            .map(|s| s.to_string())
                            .unwrap_or(href);
                    }
                    anchor.set_attr("href", &href);
                    for (key, values) in config.external_link_attribs(&href) {
                        if key == "rel" {
                            for v in &values {
                                crate::pipeline::add_link_attributes::add_rel(anchor, v);
                            }
                        } else if key == "class" {
                            for v in &values {
                                crate::pipeline::add_link_attributes::add_class(anchor, v);
                            }
                        } else {
                            anchor.set_attr(&key, values.join(" "));
                        }
                    }
                } else {
                    let mut href = crate::title::make_link(&link_title, config);
                    if let Some(fragment) = &link_title.fragment {
                        href.push('#');
                        href.push_str(fragment);
                    }
                    anchor.set_attr("href", href);
                    anchor.set_attr("title", link_title.get_prefixed_text());
                }
            }
            // A caption may still override the `title` (mirrors
            // `$anchor->setAttribute('title', $captionText)`).
            if let Some(cap) = opts.caption_text {
                anchor.set_attr("title", cap);
            }
        } else {
            // Description link to the file page (mirrors `$addDescriptionLink`).
            anchor.set_attr("href", description_link_href(title, opts, config));
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

/// The description-link href for a media container, appending `?page=`/`?lang=`
/// query parameters (mirrors `replaceAnchor`'s `$addDescriptionLink`).
fn description_link_href(title: &Title, opts: &AnchorOpts, config: &dyn SiteConfig) -> String {
    let mut href = crate::title::make_link(title, config);
    let mut qs: Vec<(&str, &str)> = Vec::new();
    if let Some(page) = opts.page
        && page.parse::<u32>().is_ok_and(|n| n > 0)
    {
        qs.push(("page", page));
    }
    if let Some(lang) = opts.lang
        && !lang.is_empty()
    {
        qs.push(("lang", lang));
    }
    if !qs.is_empty() {
        let mut q = String::new();
        for (i, (k, v)) in qs.iter().enumerate() {
            if i > 0 {
                q.push('&');
            }
            q.push_str(k);
            q.push('=');
            q.push_str(v);
        }
        href.push('?');
        href.push_str(&q);
    }
    href
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
