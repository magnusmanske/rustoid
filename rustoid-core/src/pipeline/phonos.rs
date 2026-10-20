//! The `<phonos>` extension (Phonos — pronunciation audio buttons).
//!
//! A legacy parser hook, so the wiki serves exactly what its
//! `Phonos::renderPhonos` builds: a `PhonosButton` (an OOUI `ButtonWidget`)
//! inside a `<span class="ext-phonos">`, followed by an attribution `<sup>`.
//!
//! The corpus uses only the `<phonos file="…">` form (no `ipa`/`text`/`label`),
//! so the button is always frameless with the `volumeUp` icon and no visible
//! label — the `PhonosButton` config in `ButtonWidgetConfig` below. The pure
//! half lives here; fetching the file's media info (and its mp3 transcode URL)
//! is the caller's job, since it needs the async data source.

use crate::dom::node::{ElementKind, Node};
use crate::pipeline::ooui::{ButtonWidget, Json, OouiCounter};
use crate::traits::{FileInfo, SiteConfig};

/// The `<phonos>` tag's arguments, merged with `renderPhonos`'s defaults.
#[derive(Debug, Clone, Default)]
pub struct PhonosArgs {
    /// `file` — the file name without its namespace prefix.
    pub file: String,
    pub ipa: String,
    pub text: String,
    /// `lang` — defaults to the content language.
    pub lang: String,
    pub wikibase: String,
    /// `class` — split on spaces; defaults to the `phonos-button-classes`
    /// message (the corpus value is `noexcerpt`).
    pub classes: Vec<String>,
}

impl PhonosArgs {
    /// Build from the tag's attribute pairs (`name` → `value`), applying the
    /// defaults `renderPhonos` uses. `label` is dropped, as Phonos drops it
    /// (T340905).
    pub fn from_attrs(attrs: &[(String, String)], default_lang: &str) -> Self {
        let mut args = Self {
            lang: default_lang.to_string(),
            classes: vec!["noexcerpt".to_string()],
            ..Default::default()
        };
        for (key, value) in attrs {
            match key.as_str() {
                "file" => args.file = value.clone(),
                "ipa" => args.ipa = value.clone(),
                "text" => args.text = value.clone(),
                "lang" => args.lang = value.clone(),
                "wikibase" => args.wikibase = value.clone(),
                "class" => {
                    args.classes = value
                        .split(' ')
                        .filter(|c| !c.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
        args
    }
}

/// `$file->getTitle()->getText()` for the resolved file: the wiki rewrites a
/// file redirect *and* capitalizes the first letter, and the API reports neither
/// on `title` (it echoes the requested title). The description URL does carry
/// it, so it is read from there (percent-decoding, dropping the `File:`
/// namespace and turning the URL's underscores back into spaces).
pub fn resolved_file_text(info: &FileInfo) -> String {
    let source = if info.description_url.is_empty() {
        &info.title
    } else {
        &info.description_url
    };
    let last = source
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let decoded = percent_decode(last);
    let without_ns = decoded.strip_prefix("File:").unwrap_or(&decoded);
    without_ns.replace('_', " ")
}

/// Decode `%XX` escapes in a URL component.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `Phonos::getFileUrl`: an mp3 transcode when the file is not already mp3 and
/// TimedMediaHandler produced one, otherwise the original URL.
pub fn file_url(info: &FileInfo) -> String {
    if info.mime_type != "audio/mpeg"
        && let Some(mp3) = info
            .derivatives
            .iter()
            .find(|d| d.transcodekey.as_deref() == Some("mp3"))
    {
        return mp3.src.clone();
    }
    info.file_url.clone()
}

/// Render the extension to its `<span class="ext-phonos">` node.
///
/// `info` is the resolved file's media info; when the file is missing the hook
/// throws, and the caller leaves the tag to the error path (rustoid renders the
/// served error markup there). `about` and `data-mw` are added by the caller,
/// as for every other extension.
pub fn render(
    args: &PhonosArgs,
    info: &FileInfo,
    label: Option<&Node>,
    config: &dyn SiteConfig,
    counter: &OouiCounter,
    strip_data_parsoid: bool,
) -> Node {
    // `handleExistingFile` rewrites `data.file` to the resolved target's text.
    let resolved = resolved_file_text(info);
    let href = file_url(info);

    let data = Json::Object(vec![
        ("ipa".to_string(), Json::Str(args.ipa.clone())),
        ("text".to_string(), Json::Str(args.text.clone())),
        ("lang".to_string(), Json::Str(args.lang.clone())),
        ("wikibase".to_string(), Json::Str(args.wikibase.clone())),
        ("file".to_string(), Json::Str(resolved.clone())),
    ]);

    let button = ButtonWidget {
        href: Some(href),
        rel: vec!["nofollow".to_string()],
        framed: true, // PhonosButton forces frameless; kept as the OOUI default.
        icon: None,
        label: label.cloned(),
        data: Some(data),
        classes: args.classes.clone(),
        aria_label: Some("Play audio".to_string()),
        title: None,
        disabled: false,
        phonos_button: true,
        error: false,
    }
    .into_node(counter, strip_data_parsoid);

    let sup = attribution_sup(&resolved, config);

    let mut wrapper = Node::element(ElementKind::Other("span".to_string()));
    wrapper.set_attr("class", "ext-phonos");
    wrapper.push_child(button);
    wrapper.push_child(sup);
    wrapper
}

/// The attribution `<sup>`: a link to the file's description page showing the
/// `ⓘ` icon (`Phonos::addAttributionLink`).
fn attribution_sup(file_text: &str, config: &dyn SiteConfig) -> Node {
    let mut sup = Node::element(ElementKind::Other("sup".to_string()));
    sup.set_attr(
        "class",
        "ext-phonos-attribution noexcerpt navigation-not-searchable",
    );
    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("href", local_file_url(file_text, config));
    a.set_attr("title", format!("File:{file_text}"));
    a.push_child(Node::text("ⓘ"));
    sup.push_child(a);
    sup
}

/// The wiki's `/wiki/File:…` URL for a file, matching `LinkRenderer::makeLink`
/// (which uses the article path and the title's DBkey, with spaces as
/// underscores).
fn local_file_url(file_text: &str, config: &dyn SiteConfig) -> String {
    let dbkey = file_text.replace(' ', "_");
    let encoded = wf_urlencode(&format!("File:{dbkey}"));
    config.article_path().replace("$1", &encoded)
}

/// MediaWiki's `wfUrlencode`: `rawurlencode` (unreserved `A-Za-z0-9-_.~` kept)
/// followed by unescaping a handful of sub-delims and `; @ : /`, so a file name
/// with non-ASCII is percent-encoded but `(`/`)`/`:` are not.
fn wf_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b';'
            | b'@'
            | b'$'
            | b'!'
            | b'*'
            | b'('
            | b')'
            | b','
            | b'/'
            | b'?'
            | b':'
            | b'#' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn attrs_default_to_the_content_language_and_noexcerpt() {
        let a = PhonosArgs::from_attrs(&[("file".into(), "X.ogg".into())], "en");
        assert_eq!(a.file, "X.ogg");
        assert_eq!(a.lang, "en");
        assert_eq!(a.classes, vec!["noexcerpt".to_string()]);
        assert_eq!(a.ipa, "");
    }

    #[test]
    fn a_class_attribute_replaces_the_default() {
        let a = PhonosArgs::from_attrs(
            &[
                ("file".into(), "X.ogg".into()),
                ("class".into(), "one two".into()),
            ],
            "en",
        );
        assert_eq!(a.classes, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn the_transcode_url_is_preferred_when_not_already_mp3() {
        let mut info = FileInfo {
            mime_type: "audio/ogg".to_string(),
            file_url: "//orig.ogg".to_string(),
            ..Default::default()
        };
        assert_eq!(file_url(&info), "//orig.ogg");
        info.derivatives = vec![crate::traits::FileDerivative {
            src: "//t.mp3".to_string(),
            transcodekey: Some("mp3".to_string()),
            ..Default::default()
        }];
        assert_eq!(file_url(&info), "//t.mp3");
    }

    #[test]
    fn the_local_file_url_uses_the_article_path_and_underscores() {
        let config = MockSiteConfig::default();
        let url = local_file_url("De-Albert Einstein.oga", &config);
        assert!(url.contains("File:De-Albert_Einstein.oga"), "{url}");
    }

    #[test]
    fn the_local_file_url_percent_encodes_non_ascii_but_keeps_sub_delims() {
        let config = MockSiteConfig::default();
        let url = local_file_url("République Française (versión alternativa).wav", &config);
        assert_eq!(
            url,
            "/wiki/File:R%C3%A9publique_Fran%C3%A7aise_(versi%C3%B3n_alternativa).wav"
        );
    }

    #[test]
    fn the_resolved_text_comes_from_the_description_url() {
        let info = FileInfo {
            title: "File:Albert Einstein german.ogg".to_string(),
            description_url: "https://commons.wikimedia.org/wiki/File:De-Albert_Einstein.oga"
                .to_string(),
            ..Default::default()
        };
        assert_eq!(resolved_file_text(&info), "De-Albert Einstein.oga");
    }

    #[test]
    fn the_resolved_text_falls_back_to_the_title() {
        let info = FileInfo {
            title: "File:De-Pfannkuchen.ogg".to_string(),
            ..Default::default()
        };
        assert_eq!(resolved_file_text(&info), "De-Pfannkuchen.ogg");
    }
}
