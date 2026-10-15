//! File media metadata, fetched at a requested display size.
//!
//! `DataSource::get_file_info` needs more than a file's natural size: the
//! rendered `<img>` uses the width the wiki returns for the size the media
//! option asked for (`thumbwidth`), and Parsoid sizes the request per media
//! option (`AddMediaInfo::run` passes `$dims` through `DataAccess::getFileInfo`).
//! So the request carries the display size and the answer carries the thumbnail
//! URL and its returned dimensions — the shape `AddMediaInfo`'s `handleSize` and
//! `getPath` expect.
//!
//! One file is one request: unlike link existence there is no batching worth
//! having, because the size differs per request anyway.

use std::collections::BTreeMap;

use rustoid_core::traits::{FileDerivative, FileInfo, FileTrack};

use crate::error::{CompareError, Result};
use crate::wire::WikiClient;

/// Fetch one file's media info at a requested display size.
///
/// Returns `None` only when the wiki has no `imageinfo` for the title, which
/// `AddMediaInfo` turns into the broken-media markup plus
/// `apierror-filedoesnotexist`. Note that a Commons-shared file is reported
/// `missing: true` (no local description page) while still carrying
/// `imageinfo`; the presence of `imageinfo`, not the `missing` flag, is what
/// says the file exists.
pub async fn file_info(
    client: &WikiClient,
    title: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<Option<FileInfo>> {
    let body = client.file_info_json(title, width, height).await?;
    let parsed: ImageInfoResponse =
        serde_json::from_str(&body).map_err(|e| CompareError::Response {
            url: client.wiki().api_url(),
            message: format!("imageinfo parse: {e}"),
        })?;

    // One file is one page in the answer; a missing page (no `imageinfo`) means
    // the wiki does not have the file.
    let Some(page) = parsed.query.pages.into_iter().next() else {
        return Ok(None);
    };
    let Some(info) = page.imageinfo.and_then(|mut v| v.drain(..).next()) else {
        return Ok(None);
    };
    let responsive_urls = info
        .responsive_urls
        .into_iter()
        .filter_map(|(density, url)| url.map(|u| (density, u)))
        .collect();
    // The derivative list is a `videoinfo` answer; for a still image the prop is
    // absent (or empty), so this stays empty and the caller falls back to one
    // source built from the file itself.
    // The derivative list and the subtitle tracks are both `videoinfo` answers;
    // for a still image the prop is absent (or empty), so both stay empty.
    let video = page.videoinfo.and_then(|mut v| v.drain(..).next());
    let (derivative_rows, track_rows) = match video {
        Some(v) => (v.derivatives, v.timedtext),
        None => (Vec::new(), Vec::new()),
    };
    let derivatives = derivative_rows
        .into_iter()
        .map(|d| FileDerivative {
            src: d.src,
            mime: d.mime.unwrap_or_default(),
            width: d.width.unwrap_or(0) as u32,
            height: d.height.unwrap_or(0) as u32,
            transcodekey: d.transcodekey,
        })
        .collect();
    // The subtitle tracks, in the API's order. `AddMediaInfo::addTracks` emits
    // them in that order after the `<source>`s, so the order is load-bearing.
    let timedtext = track_rows
        .into_iter()
        .map(|t| FileTrack {
            kind: t.kind,
            mime: t.mime,
            src: collapse_duplicate_origin(&t.src),
            srclang: t.srclang,
            label: t.label,
            title: t.title.unwrap_or_default(),
            dir: t.dir,
        })
        .collect();
    let mut result = FileInfo {
        title: page.title.unwrap_or_default(),
        mime_type: info.mime.unwrap_or_default(),
        media_type: info.mediatype,
        derivatives,
        timedtext,
        size: info.size.unwrap_or(0),
        width: info.width.unwrap_or(0) as u32,
        height: info.height.unwrap_or(0) as u32,
        duration: info.duration,
        description_url: info.descriptionurl.unwrap_or_default(),
        file_url: info.url.unwrap_or_default(),
        thumb_url: info.thumburl,
        thumb_width: info.thumbwidth.map(|w| w as u32),
        thumb_height: info.thumbheight.map(|h| h as u32),
        responsive_urls,
        bad_file: page.badfile.unwrap_or(false),
    };
    // The API expands URLs and stamps its own UTM campaign; the parser wants
    // the protocol-relative, `parser`-campaign form.
    result.normalize_api_urls();
    Ok(Some(result))
}

#[derive(serde::Deserialize)]
struct ImageInfoResponse {
    query: ImageInfoQuery,
}

#[derive(serde::Deserialize)]
struct ImageInfoQuery {
    #[serde(default)]
    pages: Vec<ImageInfoPage>,
}

#[derive(serde::Deserialize)]
struct ImageInfoPage {
    #[serde(default)]
    title: Option<String>,
    /// `prop=imageinfo` puts `badfile` at the page level.
    #[serde(default)]
    badfile: Option<bool>,
    #[serde(default)]
    imageinfo: Option<Vec<ImageInfo>>,
    /// `prop=videoinfo` — present only for an audio/video file.
    #[serde(default)]
    videoinfo: Option<Vec<VideoInfo>>,
}

#[derive(serde::Deserialize)]
struct VideoInfo {
    #[serde(default)]
    derivatives: Vec<Derivative>,
    /// TimedMediaHandler's subtitle list (`viprop=timedtext`); empty for a file
    /// with no subtitles and for a wiki without the extension.
    #[serde(default)]
    timedtext: Vec<TimedText>,
}

#[derive(serde::Deserialize)]
struct TimedText {
    #[serde(default)]
    kind: String,
    #[serde(default, rename = "type")]
    mime: String,
    #[serde(default)]
    src: String,
    #[serde(default)]
    srclang: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    dir: String,
}

/// Collapse the duplicate `origin=*` the API appends to a shared-repo file's
/// track URL.
///
/// `TextHandler::getFullURL` appends `origin=*` once for a file that is not local
/// to the asking wiki, and the served HTML carries it once. The API path appends
/// it a second time when it proxies the shared repo's answer, so a request to
/// enwiki for a Commons file answers `…&origin=%2A&origin=%2A` where Parsoid
/// emits `…&origin=%2A`. Dropping the repeat reproduces the served URL for both a
/// local file (no `origin` at all) and a shared one.
fn collapse_duplicate_origin(src: &str) -> String {
    let Some((base, query)) = src.split_once('?') else {
        return src.to_string();
    };
    let mut origin: Option<&str> = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|part| {
            if part.starts_with("origin=") {
                origin = Some(part);
                false
            } else {
                true
            }
        })
        .collect();
    let Some(origin) = origin else {
        return src.to_string();
    };
    format!("{base}?{}&{origin}", kept.join("&"))
}

#[derive(serde::Deserialize)]
struct Derivative {
    #[serde(default)]
    src: String,
    #[serde(default, rename = "type")]
    mime: Option<String>,
    #[serde(default)]
    width: Option<u64>,
    #[serde(default)]
    height: Option<u64>,
    #[serde(default)]
    transcodekey: Option<String>,
}

#[derive(serde::Deserialize)]
struct ImageInfo {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    descriptionurl: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    /// MediaWiki's media class (`AUDIO`, `VIDEO`, `BITMAP`, `DRAWING`, …).
    #[serde(default)]
    mediatype: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    width: Option<u64>,
    #[serde(default)]
    height: Option<u64>,
    /// Present for audio/video only.
    #[serde(default)]
    duration: Option<f64>,
    /// Present only when a display size was requested.
    #[serde(default)]
    thumburl: Option<String>,
    #[serde(default)]
    thumbwidth: Option<u64>,
    #[serde(default)]
    thumbheight: Option<u64>,
    /// Display density → thumbnail URL, present alongside a thumbnail.
    #[serde(default, rename = "responsiveUrls")]
    responsive_urls: BTreeMap<String, Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live shape of `prop=imageinfo&iiurlwidth=…`: the thumbnail URL rides
    /// with `thumbwidth`/`thumbheight` and a density map, and the page-level
    /// `badfile` is what marks a file on the bad list. Reading the wrong level
    /// (or the wrong key) silently loses the thumbnail and renders the raw file
    /// URL instead.
    #[test]
    fn a_thumb_is_read_with_its_returned_dimensions() {
        let json = r#"{"query":{"pages":[{"ns":6,"title":"File:X.jpg","imageinfo":[
            {"url":"https://u/X.jpg","descriptionurl":"https://d/X.jpg",
             "mime":"image/jpeg","mediatype":"BITMAP","size":100,"width":1000,"height":800,
             "thumburl":"https://t/250px-X.jpg","thumbwidth":250,"thumbheight":200,
             "responsiveUrls":{"2":"https://t/500px-X.jpg"}}]}]}}"#;
        let parsed: ImageInfoResponse = serde_json::from_str(json).unwrap();
        let info = parsed.query.pages[0]
            .imageinfo
            .as_ref()
            .and_then(|v| v.first())
            .unwrap();
        assert_eq!(info.thumbwidth, Some(250));
        assert_eq!(info.thumbheight, Some(200));
        assert_eq!(info.mediatype.as_deref(), Some("BITMAP"));
        assert_eq!(info.url.as_deref(), Some("https://u/X.jpg"));
        assert_eq!(
            info.responsive_urls.get("2").and_then(|u| u.as_deref()),
            Some("https://t/500px-X.jpg")
        );
    }

    /// `prop=videoinfo` rides alongside `imageinfo` for an audio/video file; its
    /// `derivatives` are the `<source>` list, and a transcode carries its key.
    #[test]
    fn derivatives_are_read_from_videoinfo() {
        let json = r#"{"query":{"pages":[{"ns":6,"title":"File:X.ogg",
            "imageinfo":[{"mime":"application/ogg","mediatype":"AUDIO"}],
            "videoinfo":[{"derivatives":[
                {"src":"https://u/X.ogg","type":"audio/ogg; codecs=\"vorbis\"","width":0,"height":0},
                {"src":"https://t/X.mp3","type":"audio/mpeg","transcodekey":"mp3","width":0,"height":0}
            ]}]}]}}"#;
        let parsed: ImageInfoResponse = serde_json::from_str(json).unwrap();
        let d = &parsed.query.pages[0].videoinfo.as_ref().unwrap()[0].derivatives;
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].mime.as_deref(), Some("audio/ogg; codecs=\"vorbis\""));
        assert!(d[0].transcodekey.is_none());
        assert_eq!(d[1].transcodekey.as_deref(), Some("mp3"));
    }

    /// The subtitle list rides in `videoinfo.timedtext`, and the local API appends
    /// `origin=*` a second time for a file on a shared repo — the served HTML
    /// carries it once.
    #[test]
    fn timedtext_is_read_and_the_duplicate_origin_collapsed() {
        let src = "https://commons.wikimedia.org/w/api.php?action=timedtext\
             &title=File%3AX.ogg&lang=am&trackformat=vtt&origin=%2A&origin=%2A";
        let json = format!(
            r#"{{"query":{{"pages":[{{"ns":6,"title":"File:X.ogg",
            "imageinfo":[{{"mime":"application/ogg","mediatype":"AUDIO"}}],
            "videoinfo":[{{"derivatives":[],"timedtext":[
                {{"src":"{src}","kind":"subtitles","type":"text/vtt",
                 "srclang":"am","dir":"ltr","label":"\u202a(am)\u202c"}}
            ]}}]}}]}}}}"#
        );
        let parsed: ImageInfoResponse = serde_json::from_str(&json).unwrap();
        let t = &parsed.query.pages[0].videoinfo.as_ref().unwrap()[0].timedtext;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, "subtitles");
        assert_eq!(t[0].mime, "text/vtt");
        assert_eq!(t[0].dir, "ltr");
        assert_eq!(t[0].srclang, "am");
        assert!(t[0].title.is_none());
        // The collapse is applied by `file_info`, not by the deserializer.
        let collapsed = collapse_duplicate_origin(&t[0].src);
        assert_eq!(collapsed.matches("origin=").count(), 1);
        assert!(collapsed.ends_with("&lang=am&trackformat=vtt&origin=%2A"));
    }

    /// A URL with no `origin` (a file local to the asking wiki) is left alone.
    #[test]
    fn a_local_track_url_is_unchanged() {
        let s = "https://en.wikipedia.org/w/api.php?action=timedtext&title=File%3AY.ogg&lang=en&trackformat=vtt";
        assert_eq!(collapse_duplicate_origin(s), s);
    }
}
