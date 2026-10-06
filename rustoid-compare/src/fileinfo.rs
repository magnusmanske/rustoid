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

use rustoid_core::traits::{FileDerivative, FileInfo};

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
    let derivatives = page
        .videoinfo
        .and_then(|mut v| v.drain(..).next())
        .map(|v| v.derivatives)
        .unwrap_or_default()
        .into_iter()
        .map(|d| FileDerivative {
            src: d.src,
            mime: d.mime.unwrap_or_default(),
            width: d.width.unwrap_or(0) as u32,
            height: d.height.unwrap_or(0) as u32,
            transcodekey: d.transcodekey,
        })
        .collect();
    let mut result = FileInfo {
        title: page.title.unwrap_or_default(),
        mime_type: info.mime.unwrap_or_default(),
        media_type: info.mediatype,
        derivatives,
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
}
