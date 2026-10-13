//! The `<imagemap>` extension (ImageMap).
//!
//! A faithful port of Parsoid's `ParsoidImageMap::sourceToDom` (extension
//! `mediawiki-extensions-ImageMap`). The tag body is a small line language:
//!
//! ```text
//! File:Solar System Template 2.png|alt=The Sun, the planets
//! # a comment
//! rect   0  0 90 35 [[Sun|The Sun]]
//! circle 112 18 6   [[Mercury (planet)|Mercury]]
//! desc bottom-right
//! ```
//!
//! The first real line is the image; every other line is one `<area>` of the
//! image map (or a `desc` placement). This module holds the *pure* half — the
//! line grammar, the map name, and the `<area>`/`<map>` node construction. The
//! rendering half (the media, the per-line links, the coordinate scaling) lives
//! in [`crate::pipeline::parser`], because it needs the parser's async
//! sub-pipelines.

use crate::dom::node::{ElementKind, Node};

/// Where a `desc` line puts the image-map description, if any.
///
/// The canonical wire strings are `imagemap_desc_types`
/// (`top-right, bottom-right, bottom-left, top-left, none`); only the canonical
/// spelling is handled, since that is the enwiki content-language value and an
/// i18n variant would need the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DescPlacement {
    TopRight,
    #[default]
    BottomRight,
    BottomLeft,
    TopLeft,
    None,
}

impl DescPlacement {
    /// The `mw-ext-imagemap-desc-<suffix>` class suffix, or `None` for `none`.
    pub fn class_suffix(self) -> Option<&'static str> {
        match self {
            Self::TopRight => Some("top-right"),
            Self::BottomRight => Some("bottom-right"),
            Self::BottomLeft => Some("bottom-left"),
            Self::TopLeft => Some("top-left"),
            Self::None => None,
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "top-right" => Some(Self::TopRight),
            "bottom-right" => Some(Self::BottomRight),
            "bottom-left" => Some(Self::BottomLeft),
            "top-left" => Some(Self::TopLeft),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// The area shapes Parsoid accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Covers the whole image; carries no `shape`/`coords` and, when it is the
    /// only area, becomes the image's own link rather than an `<area>`.
    Default,
    Rect,
    Circle,
    Poly,
}

impl Shape {
    /// The wire name, or `None` for `default` (which omits the attribute).
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Rect => Some("rect"),
            Self::Circle => Some("circle"),
            Self::Poly => Some("poly"),
        }
    }

    /// How many coordinates the shape requires (a multiple of two for `poly`).
    fn min_coords(self) -> usize {
        match self {
            Self::Default => 0,
            Self::Rect => 4,
            Self::Circle => 3,
            Self::Poly => 1,
        }
    }

    /// Whether negative coordinates are allowed (`poly` only).
    fn allows_negative(self) -> bool {
        matches!(self, Self::Poly)
    }
}

/// One non-image line of the body: an area, or the `desc` placement.
#[derive(Debug, Clone, PartialEq)]
pub struct AreaLine {
    pub shape: Shape,
    /// Raw (unscaled) coordinates, as written.
    pub coords: Vec<f64>,
    /// The `[[…]]`/`[…]` link wikitext, exactly as written.
    pub link: String,
    /// 1-based line number, for error messages.
    pub line: usize,
}

/// The parsed body.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    /// The image title, e.g. `File:Foo.png`.
    pub image: String,
    /// The image's options, without the leading `|` (e.g. `alt=…`). Empty when
    /// the first line had no options.
    pub options: String,
    pub desc: DescPlacement,
    pub areas: Vec<AreaLine>,
}

/// Parse the tag body. Mirrors the line loop of `sourceToDom`, but stops at the
/// grammar: rendering the image and the links is the caller's job.
///
/// Returns an error message key (Parsoid's `imagemap_*` keys) on a malformed
/// body, so the caller can leave the tag visible rather than emit a wrong map.
pub fn parse_body(src: &str) -> Result<Body, String> {
    let mut image: Option<String> = None;
    let mut options = String::new();
    let mut desc = DescPlacement::default();
    let mut areas = Vec::new();

    for (idx, raw) in src.split('\n').enumerate() {
        let line_num = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if image.is_none() {
            // First real line: `Image:Foo.png | …`.
            let (img, opts) = match line.split_once('|') {
                Some((img, opts)) => (img.trim_end(), opts),
                None => (line, ""),
            };
            image = Some(img.to_string());
            options = opts.trim_end().to_string();
            continue;
        }

        // A `desc <type>` placement line.
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or_default();
        if cmd == "desc" {
            let ty = words.next().unwrap_or_default();
            match DescPlacement::from_wire(ty) {
                Some(d) => desc = d,
                None => return Err("imagemap_invalid_desc".to_string()),
            }
            continue;
        }

        // The link is everything from the first `[`; what precedes it is the
        // shape spec.
        let Some(bracket) = line.find('[') else {
            return Err(format!("imagemap_no_link line {line_num}"));
        };
        let link = line[bracket..].trim().to_string();
        let spec = &line[..bracket];

        let mut spec_words = spec.split_whitespace();
        let shape = match spec_words.next().unwrap_or_default() {
            "default" => Shape::Default,
            "rect" => Shape::Rect,
            "circle" => Shape::Circle,
            "poly" => Shape::Poly,
            other => {
                let _ = other;
                return Err(format!("imagemap_unrecognised_shape line {line_num}"));
            }
        };

        let mut coords = Vec::new();
        for tok in spec_words {
            let Ok(value) = tok.parse::<f64>() else {
                return Err(format!("imagemap_invalid_coord line {line_num}"));
            };
            if value > 1e9 || (!shape.allows_negative() && value < 0.0) {
                return Err(format!("imagemap_invalid_coord line {line_num}"));
            }
            coords.push(value);
        }
        if coords.len() < shape.min_coords() {
            return Err(format!("imagemap_missing_coord line {line_num}"));
        }
        if shape == Shape::Poly && coords.len() % 2 != 0 {
            return Err(format!("imagemap_poly_odd line {line_num}"));
        }

        areas.push(AreaLine {
            shape,
            coords,
            link,
            line: line_num,
        });
    }

    let Some(image) = image else {
        return Err("imagemap_no_image".to_string());
    };
    Ok(Body {
        image,
        options,
        desc,
        areas,
    })
}

/// The `<map>` name: `ImageMap_` + the first 16 hex digits of the `md5` of the
/// serialized `<area>` children, so two identical maps on one page share a name
/// and the browser matches the right one (T18471).
pub fn map_name(inner_html: &str) -> String {
    let digest = md5::compute(inner_html.as_bytes());
    let hex = format!("{digest:x}");
    format!("ImageMap_{}", &hex[..16])
}

/// Build one `<area>`, with the attributes in the exact order
/// `ParsoidImageMap::sourceToDom` sets them — which matters twice: for the
/// served bytes, and because the `<map>`'s name hashes this serialization.
#[allow(clippy::too_many_arguments)]
pub fn area_node(
    href: &str,
    external: bool,
    ext_rel: Option<&str>,
    ext_target: Option<&str>,
    shape: Shape,
    coords: &[i64],
    alt: &str,
) -> Node {
    let mut area = Node::element(ElementKind::Other("area".to_string()));
    area.set_attr("href", href);
    if external {
        area.set_attr("class", "plainlinks");
        if let Some(rel) = ext_rel {
            area.set_attr("rel", rel);
        }
        if let Some(target) = ext_target {
            area.set_attr("target", target);
        }
    }
    if let Some(shape) = shape.wire() {
        area.set_attr("shape", shape);
    }
    if !coords.is_empty() {
        area.set_attr(
            "coords",
            coords
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    // `alt` is omitted for `default` (the whole-image link); `title` is set
    // whenever there is link text.
    if shape != Shape::Default && !alt.is_empty() {
        area.set_attr("alt", alt);
    }
    if !alt.is_empty() {
        area.set_attr("title", alt);
    }
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_is_the_image_with_options() {
        let body = parse_body("File:Foo.png|alt=Bar\nrect 0 0 10 10 [[A]]").unwrap();
        assert_eq!(body.image, "File:Foo.png");
        assert_eq!(body.options, "alt=Bar");
        assert_eq!(body.areas.len(), 1);
        assert_eq!(body.areas[0].shape, Shape::Rect);
        assert_eq!(body.areas[0].coords, vec![0.0, 0.0, 10.0, 10.0]);
        assert_eq!(body.areas[0].link, "[[A]]");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let body = parse_body("\n# comment\nFile:Foo.png\n\n#c\ncircle 1 2 3 [[A]]\n").unwrap();
        assert_eq!(body.image, "File:Foo.png");
        assert_eq!(body.areas.len(), 1);
        assert_eq!(body.areas[0].shape, Shape::Circle);
    }

    #[test]
    fn desc_sets_the_placement_and_defaults_to_bottom_right() {
        assert_eq!(
            parse_body("File:F.png").unwrap().desc,
            DescPlacement::BottomRight
        );
        let body = parse_body("File:F.png\ndesc top-left").unwrap();
        assert_eq!(body.desc, DescPlacement::TopLeft);
        assert_eq!(body.desc.class_suffix(), Some("top-left"));
        assert!(parse_body("File:F.png\ndesc bogus").is_err());
    }

    #[test]
    fn shape_coordinate_counts_are_enforced() {
        assert!(parse_body("File:F.png\nrect 0 0 10 [[A]]").is_err());
        assert!(parse_body("File:F.png\ncircle 0 0 [[A]]").is_err());
        assert!(parse_body("File:F.png\npoly 0 0 1 [[A]]").is_err());
        assert!(parse_body("File:F.png\npoly 0 0 1 1 [[A]]").is_ok());
        assert!(parse_body("File:F.png\nblob 0 0 [[A]]").is_err());
        // Only `poly` accepts negatives.
        assert!(parse_body("File:F.png\nrect -1 0 1 1 [[A]]").is_err());
        assert!(parse_body("File:F.png\npoly -1 0 1 1 [[A]]").is_ok());
    }

    #[test]
    fn a_line_without_a_link_is_an_error() {
        assert!(parse_body("File:F.png\nrect 0 0 1 1").is_err());
        assert!(parse_body("").is_err());
    }

    #[test]
    fn the_map_name_is_md5_prefixed_and_stable() {
        // `ImageMap_` + first 16 hex digits.
        let name = map_name("<area href=\"./Sun\" shape=\"rect\" coords=\"0,0,90,35\"/>");
        assert!(name.starts_with("ImageMap_"), "{name}");
        assert_eq!(name.len(), "ImageMap_".len() + 16);
        assert_eq!(
            name,
            map_name("<area href=\"./Sun\" shape=\"rect\" coords=\"0,0,90,35\"/>")
        );
    }
}
