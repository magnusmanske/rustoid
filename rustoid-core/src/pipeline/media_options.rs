//! Media option parsing — faithful port of the media-option subset of PHP
//! Parsoid's `WikiLinkHandler` (`getOptionInfo`, `getFormat`, `getWrapperInfo`,
//! `getUsed`) plus the `Consts::$Media` option tables.
//!
//! These helpers classify image/file options (e.g. `thumb`, `200px`, `right`)
//! into canonical keys and determine the wrapper element and classes.

use crate::traits::{MagicWordMap, SiteConfig};

/// The set of horizontal alignments (from PHP's HORIZONTAL_ALIGNS).
pub const HORIZONTAL_ALIGNS: &[&str] = &["left", "right", "center", "none"];

/// The set of vertical alignments (from PHP's VERTICAL_ALIGNS).
pub const VERTICAL_ALIGNS: &[&str] = &[
    "baseline",
    "sub",
    "super",
    "top",
    "text_top",
    "middle",
    "bottom",
    "text_bottom",
];

/// Simple media options: canonical magic word → group key (from
/// `Consts::$Media['SimpleOptions']`).
fn simple_options(canonical: &str) -> Option<&'static str> {
    match canonical {
        // halign
        "img_left" | "img_right" | "img_center" | "img_none" => Some("halign"),
        // valign
        "img_baseline" | "img_sub" | "img_super" | "img_top" | "img_text_top" | "img_middle"
        | "img_bottom" | "img_text_bottom" => Some("valign"),
        // format
        "img_border" => Some("border"),
        "img_frameless" | "img_framed" | "img_thumbnail" => Some("format"),
        // upright
        "img_upright" => Some("upright"),
        // timedmedia
        "timedmedia_loop" => Some("loop"),
        "timedmedia_muted" => Some("muted"),
        _ => None,
    }
}

/// Prefix media options: canonical magic word → group key (from
/// `Consts::$Media['PrefixOptions']`).
fn prefix_options(canonical: &str) -> Option<&'static str> {
    match canonical {
        "img_link" => Some("link"),
        "img_alt" => Some("alt"),
        "img_page" => Some("page"),
        "img_lang" => Some("lang"),
        "img_upright" => Some("upright"),
        "img_width" => Some("width"),
        "img_class" => Some("class"),
        "img_manualthumb" => Some("manualthumb"),
        "timedmedia_thumbtime" => Some("thumbtime"),
        "timedmedia_starttime" => Some("start"),
        "timedmedia_endtime" => Some("end"),
        "timedmedia_disablecontrols" => Some("disablecontrols"),
        _ => None,
    }
}

/// The canonical image format names (used by `getFormat`).
pub fn is_block_format(format: &str) -> bool {
    matches!(format, "thumbnail" | "manualthumb" | "framed")
}

/// The result of classifying a media option. Mirrors the PHP `getOptionInfo`
/// return value `{ck, v, ak, s}`.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionInfo {
    /// Canonical key for the option (the "group").
    pub ck: String,
    /// Value of the option (short canonical name or parsed value).
    pub v: String,
    /// Aliased key (the original option text).
    pub ak: String,
    /// Whether it's a simple option (no separate value).
    pub s: bool,
}

/// Find the canonical magic word (from the site config) whose alias matches
/// the given option text. Mirrors `SiteConfig::getMagicWordForMediaOption`.
fn canonical_magic_word_for_option(magic_words: &MagicWordMap, opt_text: &str) -> Option<String> {
    let opt_lower = opt_text.to_lowercase();
    for (canonical, entry) in magic_words {
        if (entry.canonical.starts_with("img_") || entry.canonical.starts_with("timedmedia_"))
            && entry.aliases.iter().any(|a| a.to_lowercase() == opt_lower)
        {
            return Some(canonical.clone());
        }
    }
    None
}

/// Strip the `img_` / `timedmedia_` canonical name prefix. Mirrors PHP's
/// `shortCanonicalOption`.
fn short_canonical_option(canonical: &str) -> String {
    canonical
        .strip_prefix("img_")
        .or_else(|| canonical.strip_prefix("timedmedia_"))
        .unwrap_or(canonical)
        .to_string()
}

/// Classify a media option string. Mirrors PHP's `getOptionInfo` for the
/// simple-option and width prefix cases.
pub fn get_option_info(config: &dyn SiteConfig, opt_str: &str) -> Option<OptionInfo> {
    let o_text = opt_str.trim();
    let canonical = canonical_magic_word_for_option(config.magic_words(), o_text)?;
    let short = short_canonical_option(&canonical);

    // Simple option?
    if let Some(group) = simple_options(&canonical) {
        return Some(OptionInfo {
            ck: group.to_string(),
            v: short,
            ak: o_text.to_string(),
            s: true,
        });
    }

    // Prefix option?
    if let Some(group) = prefix_options(&canonical) {
        // For width, match `\d+(x\d+)?px?`.
        if group == "width" {
            if let Some(dim) = parse_media_dimension(o_text) {
                return Some(OptionInfo {
                    ck: "width".to_string(),
                    v: dim,
                    ak: o_text.to_string(),
                    s: false,
                });
            }
            return None;
        }
        // For other prefix options, the value is everything after the alias.
        // (We don't yet parse arbitrary `alt=`, `lang=`, etc., faithfully; the
        // simple `=value` syntax is handled via `key=value`.)
        if let Some((key, val)) = o_text.split_once('=')
            && (key.trim() == short || {
                // Accept the localized alias followed by '='.
                config
                    .magic_words()
                    .get(&canonical)
                    .map(|e| e.aliases.iter().any(|a| a == key.trim()))
                    .unwrap_or(false)
            })
        {
            return Some(OptionInfo {
                ck: group.to_string(),
                v: val.trim().to_string(),
                ak: o_text.to_string(),
                s: false,
            });
        }
        return None;
    }

    None
}

/// Parse a media dimension string like `200`, `200px`, `200x300`, `200x300px`.
/// Mirrors the essential part of `Utils::parseMediaDimensions`.
fn parse_media_dimension(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_suffix("px").unwrap_or(s);
    // Match one or two integers separated by 'x'.
    let mut parts = s.split('x');
    let first = parts.next()?.trim();
    if first.is_empty() || !first.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match parts.next() {
        Some(second) => {
            let second = second.trim();
            if second.is_empty() || !second.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some(format!("{first}x{second}"))
        }
        None => Some(first.to_string()),
    }
}

/// Determine the media format. Mirrors PHP's `getFormat`.
pub fn get_format(opts: &MediaOpts) -> Option<String> {
    if opts.manualthumb.is_some() {
        return Some("manualthumb".to_string());
    }
    opts.format.clone()
}

/// A collection of parsed media options (mirrors PHP's `$opts` array).
#[derive(Debug, Default, Clone)]
pub struct MediaOpts {
    pub format: Option<String>,
    pub manualthumb: Option<String>,
    pub halign: Option<String>,
    pub valign: Option<String>,
    pub border: Option<String>,
    pub upright: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
}

/// Determine wrapper classes and inline-ness. Mirrors PHP's `getWrapperInfo`.
pub fn get_wrapper_info(opts: &MediaOpts) -> (Vec<String>, bool) {
    let format = get_format(opts);
    let mut is_inline = !format.as_deref().is_some_and(is_block_format);
    let mut classes: Vec<String> = Vec::new();

    // mw-default-size when no explicit size and not framed/manualthumb.
    let has_size = opts.width.is_some() || opts.height.is_some();
    let is_unscaled_format = matches!(format.as_deref(), Some("manualthumb") | Some("framed"));
    if !has_size && !is_unscaled_format {
        classes.push("mw-default-size".to_string());
    }

    // Border only applies to inline (thumbnail/framed/etc.) formats.
    if is_inline && opts.border.is_some() {
        classes.push("mw-image-border".to_string());
    }

    if let Some(halign) = &opts.halign
        && HORIZONTAL_ALIGNS.contains(&halign.as_str())
    {
        is_inline = false;
        classes.push(format!("mw-halign-{halign}"));
    }

    if is_inline
        && let Some(valign_opt) = &opts.valign
        && VERTICAL_ALIGNS.contains(&valign_opt.as_str())
    {
        classes.push(format!("mw-valign-{}", valign_opt.replace('_', "-")));
    }

    (classes, is_inline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn test_get_option_info_simple() {
        let config = MockSiteConfig::new();
        let info = get_option_info(&config, "thumb").unwrap();
        assert_eq!(info.ck, "format");
        assert_eq!(info.v, "thumbnail");
        assert!(info.s);

        let info = get_option_info(&config, "right").unwrap();
        assert_eq!(info.ck, "halign");
        assert_eq!(info.v, "right");
        assert!(info.s);
    }

    #[test]
    fn test_get_option_info_width() {
        // Note: our mock doesn't define img_width aliases thoroughly, so test
        // with a plain numeric string that maps via `200px` -> 'width'.
        // This test documents the parse_media_dimension behavior.
        assert_eq!(parse_media_dimension("200"), Some("200".to_string()));
        assert_eq!(parse_media_dimension("200px"), Some("200".to_string()));
        assert_eq!(
            parse_media_dimension("200x300px"),
            Some("200x300".to_string())
        );
    }

    #[test]
    fn test_get_wrapper_info() {
        let opts = MediaOpts {
            halign: Some("right".to_string()),
            ..MediaOpts::default()
        };
        let (classes, is_inline) = get_wrapper_info(&opts);
        assert!(!is_inline);
        assert!(classes.contains(&"mw-halign-right".to_string()));
    }

    #[test]
    fn test_is_block_format() {
        assert!(is_block_format("thumbnail"));
        assert!(is_block_format("framed"));
        assert!(is_block_format("manualthumb"));
        assert!(!is_block_format("frameless"));
    }
}
