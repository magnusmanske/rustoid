//! Kartographer's `<mapframe>`/`<maplink>` extensions.
//!
//! The live wiki renders these through Kartographer's Parsoid-native tag
//! handlers (`ParsoidMapFrame::sourceToDom` and the `ParsoidDomProcessor`
//! post-pass). The output is deterministic and local: a link carrying the map
//! arguments, an `<img>` whose URL is built from them, and a group id that is a
//! SHA-1 of the normalized GeoJSON body. Nothing here needs the map tile
//! service; only the URL is emitted.
//!
//! Both tags are implemented. `<maplink>` shares the argument validator but
//! renders a lone `<a>` (no figure/img), parses its `text` as inline wikitext,
//! and gets its counter-substituted group id from the parser.

use rustoid_ast::dom::node::{ElementKind, Node};
use serde_json::Value;

/// The map server Kartographer points its static images at.
const MAP_SERVER: &str = "https://maps.wikimedia.org";
/// `$wgKartographerStaticFullWidth` (the default in Kartographer).
const STATIC_FULL_WIDTH: u32 = 1024;
/// `$wgKartographerStyles` / `$wgKartographerDfltStyle`.
const DEFAULT_MAP_STYLE: &str = "osm-intl";
/// Properties are kept verbatim in the hash; this list only matters for the
/// wikitext-parsing properties, which do not affect the corpus bodies.
const ALIGNS: [&str; 3] = ["left", "center", "right"];

/// Everything the renderer needs from the parser.
pub struct MapFrameRequest<'a> {
    /// Tag arguments in source order (the `data-mw.attrs` echo).
    pub attrs: &'a [(String, String)],
    /// Raw GeoJSON body.
    pub body: &'a str,
    /// Page title as a prefixed DB key (`Space_→_underscore`).
    pub page_title: &'a str,
    /// Revision id of the page being rendered, for the static map URL.
    pub revision_id: Option<u64>,
    /// Page language code (`en`).
    pub page_lang: &'a str,
    /// Host part of the wiki's server URL (`en.wikipedia.org`).
    pub domain: &'a str,
    /// The extension's encapsulation id (`#mwt12`).
    pub about: &'a str,
}

/// The rendered extension element and the private group id, if the body
/// produced one.
pub struct Rendered {
    pub node: Node,
    pub group_id: Option<String>,
}

/// Everything `<maplink>` needs. The parser has already normalized the body
/// (ExternalData, parsed `title`/`description` wikitext, marker counters), since
/// both the group hash and the fallback label depend on that result.
pub struct MapLinkRequest<'a> {
    /// Tag arguments in source order (the `data-mw.attrs` echo).
    pub attrs: &'a [(String, String)],
    /// Raw GeoJSON body, for `data-mw.body.extsrc`.
    pub body: &'a str,
    /// Normalized geometries, or `None` when the body is empty/invalid.
    pub geometries: Option<&'a Value>,
    /// Rendered wikitext content, already parsed by the caller.
    pub text: Option<&'a Node>,
    /// The label `getTextWithFallback` would return when `text` is absent (the
    /// first marker symbol). Only affects the `error` class.
    pub fallback_text: Option<&'a str>,
    /// Page language code (`en`).
    pub page_lang: &'a str,
    /// The extension's encapsulation id (`#mwt12`).
    pub about: &'a str,
}

/// Render one `<mapframe>` tag. Returns `None` when the arguments are invalid
/// (Kartographer renders an error box there; no corpus page uses one).
pub fn render(req: &MapFrameRequest<'_>) -> Option<Rendered> {
    let args = Args::parse(req.attrs)?;
    let geometries = parse_geometries(req.body);
    let group_id = geometries
        .as_ref()
        .filter(|g| g.as_array().is_some_and(|a| !a.is_empty()))
        .map(|g| format!("_{}", sha1_hex(compact_json(g).as_bytes())));

    let (a, img) = build_link(req, &args, group_id.as_deref());
    let mut a = a;
    a.push_child(img);

    let node = if args.frameless {
        a
    } else {
        let mut figure = Node::element(ElementKind::Other("figure".to_string()));
        let mut classes = vec!["mw-kartographer-container".to_string()];
        if args.width == "full" {
            classes.push("mw-kartographer-full".to_string());
        }
        classes.push(format!("mw-halign-{}", args.align));
        classes.push("noviewer".to_string());
        figure.set_attr("class", classes.join(" "));
        figure.set_attr("typeof", "mw:File/Thumb");
        figure.push_child(a);
        let mut caption = Node::element(ElementKind::Other("figcaption".to_string()));
        if let Some(text) = &args.text {
            caption.push_child(Node::text(text.clone()));
        }
        figure.push_child(caption);
        figure
    };

    let mut node = node;
    node.set_attr("typeof", "mw:Extension/mapframe");
    node.set_attr("about", req.about);
    if let Some(group) = &group_id {
        node.set_attr(
            "data-overlays",
            compact_json(&Value::Array(vec![Value::String(group.clone())])),
        );
    }
    node.data_mw = Some(data_mw(req.attrs, req.body));

    Some(Rendered { node, group_id })
}

/// Render one `<maplink>` tag (port of `ParsoidMapLink::sourceToDom` plus
/// `MapLinkAttributeGenerator`). Returns `None` when the arguments are invalid.
pub fn render_maplink(req: &MapLinkRequest<'_>) -> Option<Rendered> {
    let args = MapLinkArgs::parse(req.attrs)?;
    let group_id = req
        .geometries
        .filter(|g| g.as_array().is_some_and(|a| !a.is_empty()))
        .map(|g| format!("_{}", sha1_hex(compact_json(g).as_bytes())));

    let mut class = vec!["mw-kartographer-maplink".to_string()];
    if !args.css_class.is_empty() {
        class.push(args.css_class.clone());
    }
    if !args.has_coordinates() && args.text.is_none() && req.fallback_text.is_none() {
        class.push("error".to_string());
    }

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("class", class.join(" "));
    a.set_attr("data-mw-kartographer", "maplink");
    a.set_attr("data-style", &args.map_style);
    if let (Some(lat), Some(lon)) = (args.lat, args.lon) {
        a.set_attr(
            "href",
            special_map_link(lat, lon, args.zoom, &args.language(req.page_lang)),
        );
    }
    if let Some(zoom) = args.zoom {
        a.set_attr("data-zoom", zoom.to_string());
    }
    if let (Some(lat), Some(lon)) = (args.lat, args.lon) {
        a.set_attr("data-lat", format_float(lat));
        a.set_attr("data-lon", format_float(lon));
    }
    if let Some(lang) = &args.specified_lang {
        a.set_attr("data-lang", lang);
    }
    if let Some(group) = &group_id {
        a.set_attr(
            "data-overlays",
            compact_json(&Value::Array(vec![Value::String(group.clone())])),
        );
    }
    if let Some(text) = req.text {
        for child in text.children.clone() {
            a.push_child(child);
        }
    }
    a.set_attr("typeof", "mw:Extension/maplink");
    a.set_attr("about", req.about);
    a.data_mw = Some(data_mw_named("maplink", req.attrs, req.body));

    Some(Rendered { node: a, group_id })
}

/// `<maplink>`'s validated arguments (the shared subset of
/// `MapTagArgumentValidator`).
struct MapLinkArgs {
    lat: Option<f64>,
    lon: Option<f64>,
    zoom: Option<i32>,
    map_style: String,
    text: Option<String>,
    specified_lang: Option<String>,
    css_class: String,
}

impl MapLinkArgs {
    fn parse(pairs: &[(String, String)]) -> Option<Self> {
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim());
        // `getFloat`: the regexp admits an optional leading `-`, digits and one
        // dot, and the value must stay within ±max (90 for latitude, 360 for
        // longitude). An invalid value behaves as absent.
        let float = |key: &str, max: f64| {
            get(key)
                .filter(|v| is_float_source(v))
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|f| f.is_finite() && f.abs() <= max)
        };
        let mut lat = float("latitude", 90.0);
        let mut lon = float("longitude", 360.0);
        if lat.is_some() != lon.is_some() {
            lat = None;
            lon = None;
        }
        let zoom = get("zoom")
            .filter(|v| is_int_source(v))
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|z| z.abs() <= 100);
        let map_style = match get("mapstyle") {
            Some(s) if matches!(s, "osm-intl" | "osm") => s.to_string(),
            _ => DEFAULT_MAP_STYLE.to_string(),
        };
        let specified_lang = get("lang")
            .filter(|l| is_known_language_code(l))
            .map(str::to_string);
        Some(Self {
            lat,
            lon,
            zoom,
            map_style,
            text: get("text").map(str::to_string),
            specified_lang,
            css_class: get("class")
                .filter(|c| is_css_class(c))
                .unwrap_or("")
                .to_string(),
        })
    }

    fn has_coordinates(&self) -> bool {
        self.lat.is_some() && self.lon.is_some()
    }

    fn language(&self, page_lang: &str) -> String {
        self.specified_lang
            .clone()
            .unwrap_or_else(|| page_lang.to_string())
    }
}

/// `Tag::getFloat`'s `^-?[0-9]*\.?[0-9]+$`.
fn is_float_source(value: &str) -> bool {
    let rest = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = match rest.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (rest, None),
    };
    if !whole.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match fraction {
        Some(f) => !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()),
        None => !whole.is_empty(),
    }
}

/// `Tag::getInt`'s `^-?[0-9]+$`.
fn is_int_source(value: &str) -> bool {
    let rest = value.strip_prefix('-').unwrap_or(value);
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// `MapTagArgumentValidator`'s `class` pattern `/^([a-z][\w-]*)?$/i`.
fn is_css_class(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The tag's validated arguments (mirrors `MapTagArgumentValidator`).
struct Args {
    width: String,
    height: u32,
    lat: Option<f64>,
    lon: Option<f64>,
    zoom: Option<i32>,
    map_style: String,
    align: String,
    frameless: bool,
    text: Option<String>,
    alt: String,
    specified_lang: Option<String>,
}

impl Args {
    fn parse(pairs: &[(String, String)]) -> Option<Self> {
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        // Both are required for `<mapframe>`.
        let width = validate_width(get("width")?)?;
        let height = validate_int(get("height")?)?;
        if height == 0 || height > 65535 {
            return None;
        }
        let lat = get("latitude").and_then(parse_float);
        let lon = get("longitude").and_then(parse_float);
        // Latitude and longitude must be given together.
        if lat.is_some() != lon.is_some() {
            return None;
        }
        if let Some(lat) = lat
            && !(-90.0..=90.0).contains(&lat)
        {
            return None;
        }
        let zoom = get("zoom").and_then(|v| v.trim().parse::<i32>().ok());
        if zoom.is_some_and(|z| z.abs() > 100) {
            return None;
        }
        let map_style = match get("mapstyle") {
            Some(s) if matches!(s, "osm-intl" | "osm") => s.to_string(),
            _ => DEFAULT_MAP_STYLE.to_string(),
        };
        let text = get("text").map(str::to_string);
        let alt = get("alt").unwrap_or("").to_string();
        let frameless = get("frameless").is_some() && text.as_deref().unwrap_or("").is_empty();
        let specified_lang = get("lang")
            .filter(|l| is_known_language_code(l))
            .map(str::to_string);
        let align = if width == "full" {
            "none".to_string()
        } else {
            match get("align") {
                // The content language of the corpus wiki is left-to-right, so
                // the default is `alignEnd()`, i.e. right.
                Some(a) if ALIGNS.contains(&a) => a.to_string(),
                _ => "right".to_string(),
            }
        };
        Some(Self {
            width,
            height,
            lat,
            lon,
            zoom,
            map_style,
            align,
            frameless,
            text,
            alt,
            specified_lang,
        })
    }

    fn language(&self, page_lang: &str) -> String {
        self.specified_lang
            .clone()
            .unwrap_or_else(|| page_lang.to_string())
    }
}

/// `getString('width', …)` plus the legacy percentage mapping.
fn validate_width(value: &str) -> Option<String> {
    if value == "full" {
        return Some(value.to_string());
    }
    if let Some(percent) = value.strip_suffix('%') {
        let n: u32 = percent.parse().ok()?;
        if !(1..=100).contains(&n) {
            return None;
        }
        return Some(if n == 100 {
            "full".to_string()
        } else {
            "300".to_string()
        });
    }
    let n: u32 = value.parse().ok()?;
    (1..=65535).contains(&n).then(|| value.to_string())
}

fn validate_int(value: &str) -> Option<u32> {
    value.parse().ok()
}

fn parse_float(value: &str) -> Option<f64> {
    let f: f64 = value.trim().parse().ok()?;
    f.is_finite().then_some(f)
}

/// `LanguageNameUtils::isKnownLanguageTag` for the codes the corpus could use.
fn is_known_language_code(code: &str) -> bool {
    code == "local"
        || (!code.is_empty()
            && code.len() <= 12
            && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Build the `<a>` and its `<img>`.
fn build_link(req: &MapFrameRequest<'_>, args: &Args, group_id: Option<&str>) -> (Node, Node) {
    let lang = args.language(req.page_lang);
    let css_width = if args.width == "full" {
        "100%".to_string()
    } else {
        format!("{}px", args.width)
    };

    let mut classes = vec!["mw-kartographer-map", "notheme"];
    if args.frameless {
        classes.push("noviewer");
    }
    classes.push("mw-file-description");
    if args.frameless {
        classes.push("mw-kartographer-container");
        if args.width == "full" {
            classes.push("mw-kartographer-full");
        }
        if let Some(class) = align_class(&args.align) {
            classes.push(class);
        }
    }

    let mut a = Node::element(ElementKind::Other("a".to_string()));
    a.set_attr("class", classes.join(" "));
    a.set_attr(
        "style",
        format!("min-width: {css_width}; min-height: {}px;", args.height),
    );
    a.set_attr("data-mw-kartographer", "mapframe");
    a.set_attr("data-style", &args.map_style);
    a.set_attr("data-width", &args.width);
    a.set_attr("data-height", args.height.to_string());
    if let Some(zoom) = args.zoom {
        a.set_attr("data-zoom", zoom.to_string());
    }
    if let (Some(lat), Some(lon)) = (args.lat, args.lon) {
        a.set_attr("data-lat", format_float(lat));
        a.set_attr("data-lon", format_float(lon));
    }
    if let Some(specified) = &args.specified_lang {
        a.set_attr("data-lang", specified);
    }
    if let (Some(lat), Some(lon)) = (args.lat, args.lon) {
        a.set_attr("href", special_map_link(lat, lon, args.zoom, &lang));
    }

    let img = build_image(req, args, group_id, &lang);
    (a, img)
}

fn align_class(align: &str) -> Option<&'static str> {
    match align {
        "left" => Some("floatleft"),
        "center" => Some("center"),
        "right" => Some("floatright"),
        _ => None,
    }
}

/// `SpecialMap::link`: `/wiki/Special:Map/{zoom}/{lat}/{lon}[/{lang}]`.
fn special_map_link(lat: f64, lon: f64, zoom: Option<i32>, lang: &str) -> String {
    let mut page = format!(
        "{}/{}/{}",
        zoom.unwrap_or(0),
        format_float(lat),
        format_float(lon)
    );
    if !lang.is_empty() && lang != "local" {
        page.push('/');
        page.push_str(lang);
    }
    format!("/wiki/Special:Map/{page}")
}

/// `MapFrameAttributeGenerator::prepareImgAttrs` plus the group id the
/// post-processor's `updateSrc` adds.
fn build_image(req: &MapFrameRequest<'_>, args: &Args, group_id: Option<&str>, lang: &str) -> Node {
    let static_width = if args.width == "full" {
        STATIC_FULL_WIDTH
    } else {
        args.width.parse().unwrap_or(0)
    };
    let zoom = args.zoom.map(|z| z.to_string()).unwrap_or("a".to_string());
    let lat = args
        .lat
        .map(format_float)
        .unwrap_or_else(|| "a".to_string());
    let lon = args
        .lon
        .map(format_float)
        .unwrap_or_else(|| "a".to_string());
    let base = format!(
        "{MAP_SERVER}/img/{},{zoom},{lat},{lon},{static_width}x{}",
        args.map_style, args.height
    );

    let mut params = vec![("lang", lang.to_string())];
    // The group URL parameters are added by the post-processor only when the
    // body produced a group id.
    if let Some(group) = group_id {
        params.push(("domain", req.domain.to_string()));
        params.push(("title", req.page_title.to_string()));
        if let Some(revision) = req.revision_id {
            params.push(("revid", revision.to_string()));
        }
        params.push(("groups", group.to_string()));
        params.push(("parser", "parsoid".to_string()));
    }
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", cgi_escape(value)))
        .collect::<Vec<_>>()
        .join("&");

    let mut img = Node::element(ElementKind::Other("img".to_string()));
    img.set_attr("class", "mw-file-element");
    img.set_attr("src", format!("{base}.png?{query}"));
    img.set_attr("width", static_width.to_string());
    img.set_attr("height", args.height.to_string());
    img.set_attr("decoding", "async");
    img.set_attr("srcset", format!("{base}@2x.png?{query} 2x"));
    if args.alt.is_empty() {
        // `createLangAttribute`: the alt text is localized, so the attribute is
        // replaced by the i18n marker pair.
        img.set_attr("typeof", "mw:LocalizedAttrs");
        img.set_attr(
            "data-mw-i18n",
            r#"{"alt":{"lang":"x-page","key":"kartographer-static-mapframe-alt","params":[]}}"#,
        );
    } else {
        img.set_attr("alt", &args.alt);
    }
    img
}

/// The first `marker-symbol` that names a counter (`-number…`/`-letter…`),
/// before any counter is applied. `SimpleStyleParser::findFirstMarkerSymbol`.
pub fn find_first_marker_symbol(values: &Value) -> Option<String> {
    let items = values.as_array()?;
    for item in items {
        let marker = item
            .get("properties")
            .and_then(|p| p.get("marker-symbol"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if marker.starts_with("-number") || marker.starts_with("-letter") {
            return Some(marker.to_string());
        }
        for key in ["features", "geometries"] {
            if let Some(found) = item.get(key).and_then(find_first_marker_symbol) {
                return Some(found);
            }
        }
    }
    None
}

/// Apply `SimpleStyleParser::updateMarkerSymbolCounters` in place, returning the
/// counter assigned to the *first* marker (what the DOM processor writes over a
/// `<maplink>`'s label when the tag has no `text` attribute).
pub fn update_marker_counters(values: &mut Value) -> Option<String> {
    let mut counters: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut first = None;
    walk_marker_counters(values, &mut counters, &mut first);
    first
}

fn walk_marker_counters(
    values: &mut Value,
    counters: &mut std::collections::HashMap<String, u32>,
    first: &mut Option<String>,
) {
    let Some(items) = values.as_array_mut() else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let marker = object
            .get("properties")
            .and_then(|p| p.get("marker-symbol"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let is_number = marker.starts_with("-number");
        let is_letter = marker.starts_with("-letter");
        if is_number || is_letter {
            let max = if is_number { 99 } else { 26 };
            let count = counters.entry(marker.clone()).or_insert(0);
            if *count < max {
                *count += 1;
            }
            let value = if is_number {
                count.to_string()
            } else {
                char::from(b'a' + (*count as u8 - 1)).to_string()
            };
            if first.is_none() {
                *first = Some(value.clone());
            }
            if let Some(Value::Object(properties)) = object.get_mut("properties") {
                properties.insert("marker-symbol".to_string(), Value::String(value));
            }
        }
        for key in ["features", "geometries"] {
            if let Some(child) = object.get_mut(key) {
                walk_marker_counters(child, counters, first);
            }
        }
    }
}

/// The coordinate label a `<maplink>` without `text` gets when it has no
/// marker fallback. `CoordFormatter::formatParsoidSpan` through the tag's args.
pub fn coordinate_span(attrs: &[(String, String)]) -> Option<Node> {
    let args = MapLinkArgs::parse(attrs)?;
    Some(coord_i18n_span(args.lat?, args.lon?))
}

/// `CoordFormatter::formatParsoidSpan`: an empty `mw:I18n` span whose message
/// key names the two signs and whose params are degrees/minutes/seconds.
pub fn coord_i18n_span(lat: f64, lon: f64) -> Node {
    let (lat_sign, lat_parts) = split_coord(lat);
    let (lon_sign, lon_parts) = split_coord(lon);
    let params = lat_parts
        .iter()
        .chain(lon_parts.iter())
        .map(|n| format!("{{\"num\":{n}}}"))
        .collect::<Vec<_>>()
        .join(",");
    let key = format!("kartographer-coord-lat-{lat_sign}-lon-{lon_sign}");
    let mut span = Node::element(ElementKind::Other("span".to_string()));
    span.set_attr("typeof", "mw:I18n");
    span.data_parsoid = Some("{}".to_string());
    span.set_attr(
        "data-mw-i18n",
        format!(
            "{{\"/\":{{\"lang\":\"x-page\",\"key\":{},\"params\":[{params}]}}}}",
            json_string(&key)
        ),
    );
    span
}

fn split_coord(coord: f64) -> (&'static str, [i64; 3]) {
    let value = (coord * 3600.0).round();
    let sign = if value < 0.0 { "neg" } else { "pos" };
    let value = value.abs();
    let degrees = (value / 3600.0).floor();
    let minutes = ((value - degrees * 3600.0) / 60.0).floor();
    let seconds = value - degrees * 3600.0 - minutes * 60.0;
    (sign, [degrees as i64, minutes as i64, seconds as i64])
}

/// Parse and normalize the GeoJSON body the way `SimpleStyleParser` does for
/// the hash: a non-list value becomes a one-element list, and `ExternalData`
/// services are canonicalized into a `url`.
pub fn parse_geometries(body: &str) -> Option<Value> {
    if body.trim().is_empty() {
        return None;
    }
    let mut data: Vec<Value> = match serde_json::from_str(body).ok()? {
        Value::Array(items) => items,
        other => vec![other],
    };
    for item in &mut data {
        normalize_external_data(item);
    }
    Some(Value::Array(data))
}

fn normalize_external_data(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_external_data(item);
            }
        }
        Value::Object(object)
            if object.get("type").and_then(Value::as_str) == Some("ExternalData") =>
        {
            let Some(service) = object.get("service").and_then(Value::as_str) else {
                return;
            };
            if !matches!(service, "geoshape" | "geopoint" | "geoline" | "geomask") {
                return;
            }
            let mut normalized = serde_json::Map::new();
            normalized.insert(
                "type".to_string(),
                Value::String("ExternalData".to_string()),
            );
            normalized.insert("service".to_string(), Value::String(service.to_string()));
            let mut query = vec!["getgeojson=1".to_string()];
            if let Some(ids) = object.get("ids") {
                let ids = match ids {
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(","),
                    Value::String(s) => s.split(',').map(str::trim).collect::<Vec<_>>().join(","),
                    _ => String::new(),
                };
                query.push(format!("ids={}", cgi_escape(&ids)));
            }
            if let Some(extra) = object.get("query").and_then(Value::as_str) {
                query.push(format!("query={}", cgi_escape(extra)));
            }
            // `geomask` is the inverted geoshape service; Kartotherian only
            // serves `geoshape`.
            let path = if service == "geomask" {
                "geoshape"
            } else {
                service
            };
            normalized.insert(
                "url".to_string(),
                Value::String(format!("{MAP_SERVER}/{path}?{}", query.join("&"))),
            );
            if let Some(properties) = object.get("properties") {
                normalized.insert("properties".to_string(), properties.clone());
            }
            *object = normalized;
        }
        _ => {}
    }
}

/// `FormatJson::encode( …, ALL_OK )` — compact, unescaped slashes/unicode,
/// preserving the object's key order (`preserve_order` is enabled workspace-wide).
fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn data_mw(attrs: &[(String, String)], body: &str) -> String {
    data_mw_named("mapframe", attrs, body)
}

fn data_mw_named(name: &str, attrs: &[(String, String)], body: &str) -> String {
    let pairs = attrs
        .iter()
        .map(|(key, value)| format!("{}:{}", json_string(key), json_string(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"name\":{},\"attrs\":{{{pairs}}},\"body\":{{\"extsrc\":{}}}}}",
        json_string(name),
        json_string(body)
    )
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// PHP float-to-string uses the shortest round-tripping representation.
fn format_float(value: f64) -> String {
    value.to_string()
}

/// `wfArrayToCgi` (`RFC 3986`): percent-encode everything but unreserved bytes.
fn cgi_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A local SHA-1, so the extension crate keeps its single dependency.
fn sha1_hex(data: &[u8]) -> String {
    let mut state = [
        0x67452301u32,
        0xEFCDAB89,
        0x98BADCFE,
        0x10325476,
        0xC3D2E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut words = [0u32; 80];
        for (i, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (i, word) in words.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn group_id_is_sha1_of_the_list_wrapped_geojson() {
        let body = r##"{"type":"Feature","geometry":{"coordinates":[30.099166666666665,51.38972222222222],"type":"Point"},"properties":{"title":"Chernobyl disaster","marker-color":"#5E74F3"}}"##;
        let geometries = parse_geometries(body).unwrap();
        assert_eq!(
            format!("_{}", sha1_hex(compact_json(&geometries).as_bytes())),
            "_f12de65ae136e75aea49e22427f24c5695da383e"
        );
    }

    #[test]
    fn external_data_is_canonicalized_before_hashing() {
        // Berlin's two ExternalData geoms (geoshape + geoline).
        let body = "[\n{\"properties\":{\"title\":\"Berlin\",\"fill-opacity\":0.1,\"stroke\":\"#FF0000\",\"stroke-width\":2,\"fill\":\"#606060\"},\"type\":\"ExternalData\",\"service\":\"geoshape\",\"ids\":\"Q64\"},\n{\"properties\":{\"stroke-width\":2,\"stroke\":\"#FF0000\",\"title\":\"Berlin\"},\"type\":\"ExternalData\",\"service\":\"geoline\",\"ids\":\"Q64\"}\n]";
        let geometries = parse_geometries(body).unwrap();
        assert_eq!(
            format!("_{}", sha1_hex(compact_json(&geometries).as_bytes())),
            "_6dad1445db6c1da03d17bdf993b3a98045c1f37c"
        );
    }

    #[test]
    fn empty_body_has_no_group() {
        assert_eq!(
            parse_geometries("[]").map(|g| g.as_array().unwrap().len()),
            Some(0)
        );
        assert!(parse_geometries("").is_none());
    }

    #[test]
    fn cgi_escape_matches_rawurlencode() {
        assert_eq!(cgi_escape("Chernobyl_disaster"), "Chernobyl_disaster");
        assert_eq!(cgi_escape("a b/c"), "a%20b%2Fc");
    }
}
