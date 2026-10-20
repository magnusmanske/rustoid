//! The Wikibase client's `{{#property:…}}` parser function.
//!
//! `#property` renders the *best* statements of one property, formatted as
//! escaped plain text (`DataAccessSnakFormatterFactory::TYPE_ESCAPED_PLAINTEXT`):
//!
//! - `string`, `external-id` and `commonsMedia` values are their text, wikitext
//!   escaped;
//! - a `url` is returned *unescaped*, so the parser autolinks it;
//! - a `wikibase-item` is its label in the wiki's language, falling back to the
//!   entity id;
//! - a `time` is `MwTimeIsoFormatter`'s plain rendering (a year, a month, or a
//!   day, folded by precision);
//! - a `quantity` is its localized amount plus its unit's label;
//! - a `globecoordinate` is `LatLongFormatter` with cardinal directions.
//!
//! Values are joined with `", "` (the wiki's `comma-separator`); there is no
//! "and". Lookups are done by the caller, which can fetch entities; this module
//! only parses and formats what it is handed.

use std::collections::HashMap;

use serde_json::Value;

/// Whether `text` is a property id the renderer can resolve without a label
/// lookup (`P123`; Wikidata property ids have no leading zero).
pub fn is_property_id(text: &str) -> bool {
    let mut chars = text.chars();
    if !matches!(chars.next(), Some('P' | 'p')) {
        return false;
    }
    let digits = chars.as_str();
    !digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `text` is an entity id (`Q123` or `P123`; no leading zero).
pub fn is_entity_id(text: &str) -> bool {
    let mut chars = text.chars();
    if !matches!(chars.next(), Some('Q' | 'q' | 'P' | 'p')) {
        return false;
    }
    let digits = chars.as_str();
    !digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit())
}

/// An entity's label in `language`, with Wikibase's fallback chain.
///
/// The chain on this wiki is the language itself, `mul`, then `en`; an entity
/// with no label in any of them is written as its id by the renderer.
pub fn label(entity_json: &str, language: &str) -> Option<String> {
    let entity = parse_entity(entity_json)?;
    let labels = entity.get("labels")?;
    for language in [language, "mul", "en"] {
        if let Some(value) = labels
            .get(language)
            .and_then(|l| l.get("value"))
            .and_then(Value::as_str)
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Ids whose labels the renderer needs: the values of an item-typed property
/// and the units of a quantity.
pub fn referenced_entity_ids(entity_json: &str, property: &str) -> Vec<String> {
    let Some(entity) = parse_entity(entity_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for snak in best_statements(&entity, property) {
        let Some((kind, value)) = data_value(snak) else {
            continue;
        };
        match kind {
            "wikibase-entityid" => {
                if let Some(id) = entity_id(value) {
                    out.push(id);
                }
            }
            "quantity" => {
                if let Some(id) = value
                    .get("unit")
                    .and_then(Value::as_str)
                    .and_then(entity_id_from_uri)
                {
                    out.push(id);
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Render one property's best statements as plain wikitext.
///
/// `labels` maps entity ids to their display labels; an id it does not know is
/// written as the id itself, which is what Wikibase does for an entity with no
/// label in the fallback chain.
pub fn render_property(
    entity_json: &str,
    property: &str,
    labels: &HashMap<String, String>,
) -> String {
    let Some(entity) = parse_entity(entity_json) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for snak in best_statements(&entity, property) {
        let text = render_snak(snak, labels);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(", ")
}

fn parse_entity(json: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(json).ok()?;
    // `Special:EntityData/<id>.json` wraps the entity in `{"entities": {…}}`;
    // `wbgetentities` may hand over the bare entity.
    match parsed.get("entities").and_then(Value::as_object) {
        Some(entities) => entities.values().next().cloned(),
        None => Some(parsed),
    }
}

/// `StatementList::getBestStatements`: preferred statements when there are any,
/// otherwise non-deprecated ones. The order is the entity's.
fn best_statements<'a>(entity: &'a Value, property: &str) -> Vec<&'a Value> {
    let Some(statements) = entity
        .get("claims")
        .and_then(|claims| claims.get(property))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let preferred: Vec<&Value> = statements
        .iter()
        .filter(|s| s.get("rank").and_then(Value::as_str) == Some("preferred"))
        .filter_map(|s| s.get("mainsnak"))
        .collect();
    if !preferred.is_empty() {
        return preferred;
    }
    statements
        .iter()
        .filter(|s| s.get("rank").and_then(Value::as_str) != Some("deprecated"))
        .filter_map(|s| s.get("mainsnak"))
        .collect()
}

fn data_value(snak: &Value) -> Option<(&str, &Value)> {
    if snak.get("snaktype").and_then(Value::as_str) != Some("value") {
        return None;
    }
    let datavalue = snak.get("datavalue")?;
    let kind = datavalue.get("type").and_then(Value::as_str)?;
    Some((kind, datavalue.get("value")?))
}

fn entity_id(value: &Value) -> Option<String> {
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        return Some(id.to_ascii_uppercase());
    }
    let kind = value.get("entity-type").and_then(Value::as_str)?;
    let numeric = value.get("numeric-id").and_then(Value::as_u64)?;
    let prefix = if kind == "property" { 'P' } else { 'Q' };
    Some(format!("{prefix}{numeric}"))
}

/// `http://www.wikidata.org/entity/Q25343` (or `…/P31`) to `Q25343`/`P31`.
fn entity_id_from_uri(uri: &str) -> Option<String> {
    let id = uri.rsplit('/').next()?;
    let mut chars = id.chars();
    if !matches!(chars.next()?, 'Q' | 'P') || !chars.as_str().bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(id.to_string())
}

fn render_snak(snak: &Value, labels: &HashMap<String, String>) -> String {
    let Some((kind, value)) = data_value(snak) else {
        return String::new();
    };
    let datatype = snak.get("datatype").and_then(Value::as_str).unwrap_or("");
    let text = match kind {
        "string" => value.as_str().unwrap_or("").to_string(),
        "monolingualtext" => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "time" => format_time(value),
        "wikibase-entityid" => entity_id(value)
            .map(|id| labels.get(&id).cloned().unwrap_or(id))
            .unwrap_or_default(),
        "globecoordinate" => format_coordinate(value),
        "quantity" => format_quantity(value, labels),
        _ => return String::new(),
    };
    // The dispatcher's whole point: a URL is the one value that must stay
    // unescaped, so the parser turns it into a link.
    if datatype == "url" {
        text
    } else {
        wikitext_escape(&text)
    }
}

/// `wfEscapeWikiText`, so a label that contains `[`, `{`, `=` or a quote cannot
/// become markup when `#property`'s answer is parsed.
fn wikitext_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, ch) in text.char_indices() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '[' => out.push_str("&#91;"),
            ']' => out.push_str("&#93;"),
            '{' => out.push_str("&#123;"),
            '}' => out.push_str("&#125;"),
            '|' => out.push_str("&#124;"),
            '=' => out.push_str("&#61;"),
            '*' if i == 0 => out.push_str("&#42;"),
            '#' if i == 0 => out.push_str("&#35;"),
            ':' if i == 0 => out.push_str("&#58;"),
            ';' if i == 0 => out.push_str("&#59;"),
            _ => out.push(ch),
        }
    }
    out.replace("~~", "&#126;&#126;")
}

/// `PlaintextTimeFormatter` over `MwTimeIsoFormatter`, with the calendar model
/// left off. A value its precision cannot render keeps its raw timestamp.
fn format_time(value: &Value) -> String {
    let Some(iso) = value.get("time").and_then(Value::as_str) else {
        return String::new();
    };
    let precision = value.get("precision").and_then(Value::as_i64).unwrap_or(11);
    localized_date(iso, precision).unwrap_or_else(|| iso.to_string())
}

/// `MwTimeIsoFormatter::getLocalizedDate`: the year for a year or coarser, and
/// the day or month otherwise. `None` is the formatter's `InvalidArgumentException`.
fn localized_date(iso: &str, precision: i64) -> Option<String> {
    let year = localized_year(iso, precision)?;
    if precision <= 9 {
        return Some(year);
    }
    if precision > 11 {
        return None;
    }
    let numbers: Vec<u32> = iso
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if numbers.len() < 6 {
        return None;
    }
    let month = numbers[1].max(1);
    let day = numbers[2].max(1);
    let month = *MONTHS.get(month as usize - 1)?;
    Some(if precision == 10 {
        format!("{month} {year}")
    } else {
        format!("{day} {month} {year}")
    })
}

/// `MwTimeIsoFormatter::getLocalizedYear`: the year folded into a decade,
/// century, millennium or "annum" count when the precision is coarser than a
/// year, with the calendar era appended when the stamp is BCE.
fn localized_year(iso: &str, precision: i64) -> Option<String> {
    let mut chars = iso.chars();
    let sign = chars.next()?;
    let digits: String = chars.take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let is_bce = sign == '-';
    let (shift, unshift, func, message) = match precision {
        0 => (1e9, 1.0, Rounding::Nearest, Some("Gannum")),
        1 => (1e8, 1e2, Rounding::Nearest, Some("Mannum")),
        2 => (1e7, 1e1, Rounding::Nearest, Some("Mannum")),
        3 => (1e6, 1.0, Rounding::Nearest, Some("Mannum")),
        4 => (1e5, 1e5, Rounding::Nearest, Some("annum")),
        5 => (1e4, 1e4, Rounding::Nearest, Some("annum")),
        6 => (1e3, 1.0, Rounding::Ceil, Some("millennium")),
        7 => (1e2, 1.0, Rounding::Ceil, Some("century")),
        8 => (1e1, 1e1, Rounding::Floor, Some("10annum")),
        _ => (1.0, 1.0, Rounding::Nearest, None),
    };
    let mut year = digits;
    let shifted = if shift == 1.0 && unshift == 1.0 {
        year.clone()
    } else {
        let n = year.parse::<f64>().unwrap_or(0.0);
        let n = match func {
            Rounding::Ceil => (n / shift).ceil(),
            Rounding::Floor => (n / shift).floor(),
            Rounding::Nearest => (n / shift).round(),
        };
        format!("{:.0}", n * unshift)
    };
    let message =
        if shifted.parse::<f64>() == Ok(0.0) && (precision < 9 || (is_bce && precision == 9)) {
            None
        } else {
            year = shifted;
            message
        };
    let year = year.trim_start_matches('0');
    let year = if year.is_empty() { "0" } else { year };
    let Some(message) = message else {
        return Some(if is_bce {
            format!("{year} BCE")
        } else if year.len() <= 2 {
            format!("{year} CE")
        } else {
            year.to_string()
        });
    };
    Some(match message {
        "Gannum" => format!("{year} billion years {}", if is_bce { "BCE" } else { "CE" }),
        "Mannum" => format!("{year} million years {}", if is_bce { "BCE" } else { "CE" }),
        "annum" => format!(
            "{year} year{}{}",
            if year == "1" { "" } else { "s" },
            if is_bce { " BCE" } else { " CE" }
        ),
        "millennium" => format!("{year}. millennium{}", if is_bce { " BCE" } else { "" }),
        "century" => format!("{year}. century{}", if is_bce { " BCE" } else { "" }),
        "10annum" => format!("{year}s{}", if is_bce { " BCE" } else { "" }),
        _ => year.to_string(),
    })
}

#[derive(Clone, Copy)]
enum Rounding {
    Nearest,
    Ceil,
    Floor,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `QuantityFormatter` over a `DecimalFormatter` with the wiki number localizer:
/// the amount grouped by thousands, then the unit's label.
fn format_quantity(value: &Value, labels: &HashMap<String, String>) -> String {
    let amount = value.get("amount").and_then(Value::as_str).unwrap_or("");
    let mut text = localize_amount(amount);
    let Some(unit) = value.get("unit").and_then(Value::as_str) else {
        return text;
    };
    let Some(id) = entity_id_from_uri(unit) else {
        return text;
    };
    // Q199 is Wikibase's "unitless".
    if id != "Q199"
        && let Some(label) = labels.get(&id)
    {
        text.push(' ');
        text.push_str(label);
    }
    text
}

/// Group the integer part; keep the fraction exactly as the amount spells it.
fn localize_amount(amount: &str) -> String {
    let (sign, rest) = match amount.strip_prefix('+') {
        Some(rest) => ("", rest),
        None => match amount.strip_prefix('-') {
            Some(rest) => ("-", rest),
            None => ("", amount),
        },
    };
    // An amount may use exponent notation; grouping applies to the mantissa only,
    // and a value that is not a plain decimal is left to the float spelling.
    if rest.contains(['e', 'E']) {
        return format!("{sign}{rest}");
    }
    let (integer, fraction) = match rest.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (rest, None),
    };
    let mut grouped = String::new();
    for (i, ch) in integer.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let mut out = String::from(sign);
    out.extend(grouped.chars().rev());
    if let Some(fraction) = fraction {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

/// `GlobeCoordinateFormatter` over `LatLongFormatter` with the options Wikibase
/// uses: DMS (which falls back to decimal minutes or degrees by precision),
/// cardinal directions, and spacing only between latitude and longitude.
fn format_coordinate(value: &Value) -> String {
    let latitude = value.get("latitude").and_then(Value::as_f64).unwrap_or(0.0);
    let longitude = value
        .get("longitude")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let precision = value
        .get("precision")
        .and_then(Value::as_f64)
        .filter(|p| *p > 0.0 && p.is_finite())
        .unwrap_or(1.0 / 3600.0);
    format!(
        "{}, {}",
        format_lat_lon(latitude, precision, 'N', 'S'),
        format_lat_lon(longitude, precision, 'E', 'W')
    )
}

fn format_lat_lon(degrees: f64, precision: f64, positive: char, negative: char) -> String {
    let rounded = round_degrees(degrees, precision);
    let mut coordinate = format_coordinate_value(rounded, updated_precision(precision));
    let direction = if rounded < 0.0 {
        // `makeDirectional` strips the sign and names the hemisphere.
        if let Some(stripped) = coordinate.strip_prefix('-') {
            coordinate = stripped.to_string();
        }
        negative
    } else {
        positive
    };
    coordinate.push(direction);
    coordinate
}

fn round_degrees(degrees: f64, precision: f64) -> f64 {
    let sign = if degrees > 0.0 { 1.0 } else { -1.0 };
    sign * (degrees.abs() / precision).round() * precision
}

fn updated_precision(precision: f64) -> f64 {
    if (1.0 - 1.0 / 60.0..1.0).contains(&precision) {
        return 1.0;
    }
    if (1.0 / 60.0 - 1.0 / 3600.0..1.0 / 60.0).contains(&precision) {
        return 1.0 / 60.0;
    }
    precision
}

fn format_coordinate_value(degrees: f64, precision: f64) -> String {
    if precision >= 1.0 {
        return format!(
            "{}{}",
            format_number(degrees, significant_digits(1.0, precision)),
            '°'
        );
    }
    if precision >= 1.0 / 60.0 {
        let digits = significant_digits(60.0, precision);
        let minutes = round_to(abs(degrees) * 60.0, digits);
        let whole_degrees = (minutes / 60.0).trunc();
        let minutes = minutes - whole_degrees * 60.0;
        let mut out = format!("{whole_degrees:.0}°{}'", format_number(minutes, digits));
        if degrees < 0.0 && whole_degrees + minutes > 0.0 {
            out.insert(0, '-');
        }
        return out;
    }
    let digits = significant_digits(3600.0, precision);
    let seconds = round_to(abs(degrees) * 3600.0, digits);
    let whole_minutes = (seconds / 60.0).trunc();
    let whole_degrees = (whole_minutes / 60.0).trunc();
    let seconds = seconds - whole_minutes * 60.0;
    let minutes = whole_minutes - whole_degrees * 60.0;
    let mut out = format!(
        "{whole_degrees:.0}°{}'{}\"",
        format_number(minutes, 0),
        format_number(seconds, digits)
    );
    if degrees < 0.0 && whole_degrees + minutes + seconds > 0.0 {
        out.insert(0, '-');
    }
    out
}

fn significant_digits(units_per_degree: f64, precision: f64) -> i32 {
    (-(units_per_degree * precision).log10()).ceil() as i32
}

fn round_to(number: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    (number * factor).round() / factor
}

fn format_number(number: f64, digits: i32) -> String {
    format!("{:.*}", digits.max(0) as usize, number)
}

fn abs(number: f64) -> f64 {
    number.abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIX: &str = r#"{
        "entities": {
            "Q11368": {
                "id": "Q11368",
                "claims": {
                    "P856": [
                        {"rank": "normal", "mainsnak": {"snaktype": "value",
                         "datatype": "url", "datavalue": {"type": "string",
                         "value": "https://www.opengroup.org/unix-systems"}}}
                    ],
                    "P31": [
                        {"rank": "normal", "mainsnak": {"snaktype": "value",
                         "datatype": "wikibase-item", "datavalue": {"type": "wikibase-entityid",
                         "value": {"entity-type": "item", "numeric-id": 170584, "id": "Q170584"}}}},
                        {"rank": "preferred", "mainsnak": {"snaktype": "value",
                         "datatype": "wikibase-item", "datavalue": {"type": "wikibase-entityid",
                         "value": {"entity-type": "item", "numeric-id": 1, "id": "Q1"}}}}
                    ]
                }
            }
        }
    }"#;

    fn labels() -> HashMap<String, String> {
        HashMap::from([
            ("Q170584".to_string(), "project".to_string()),
            ("Q1".to_string(), "preferred".to_string()),
        ])
    }

    #[test]
    fn a_url_is_not_escaped_and_a_property_with_no_values_is_empty() {
        assert_eq!(
            render_property(UNIX, "P856", &labels()),
            "https://www.opengroup.org/unix-systems"
        );
        assert_eq!(render_property(UNIX, "P345", &labels()), "");
    }

    #[test]
    fn only_preferred_statements_are_used() {
        assert_eq!(render_property(UNIX, "P31", &labels()), "preferred");
    }

    #[test]
    fn a_missing_label_falls_back_to_the_id() {
        assert_eq!(render_property(UNIX, "P31", &HashMap::new()), "Q1");
    }

    #[test]
    fn referenced_ids_cover_items_and_units() {
        assert_eq!(referenced_entity_ids(UNIX, "P31"), vec!["Q1"]);
    }

    #[test]
    fn a_year_precision_time_renders_its_year() {
        let value = serde_json::json!({"time": "+1969-01-01T00:00:00Z", "precision": 9});
        assert_eq!(format_time(&value), "1969");
    }

    #[test]
    fn a_day_precision_time_renders_the_day() {
        let value = serde_json::json!({"time": "+1969-07-20T00:00:00Z", "precision": 11});
        assert_eq!(format_time(&value), "20 July 1969");
    }

    #[test]
    fn a_bce_year_keeps_its_era() {
        let value = serde_json::json!({"time": "-0044-01-01T00:00:00Z", "precision": 9});
        assert_eq!(format_time(&value), "44 BCE");
    }

    #[test]
    fn a_quantity_is_grouped_and_takes_its_unit_label() {
        let value = serde_json::json!({
            "amount": "+891.12",
            "unit": "http://www.wikidata.org/entity/Q25343"
        });
        let labels = HashMap::from([("Q25343".to_string(), "square kilometre".to_string())]);
        assert_eq!(format_quantity(&value, &labels), "891.12 square kilometre");
    }

    #[test]
    fn a_coordinate_uses_the_precision_to_pick_the_format() {
        let coarse = serde_json::json!({
            "latitude": 56, "longitude": -109, "precision": 10
        });
        assert_eq!(format_coordinate(&coarse), "60°N, 110°W");
        let minutes = serde_json::json!({
            "latitude": 52.516666666666666, "longitude": 13.4, "precision": 1.0 / 60.0
        });
        assert_eq!(format_coordinate(&minutes), "52°31'N, 13°24'E");
        let seconds = serde_json::json!({
            "latitude": 39.828175, "longitude": -98.5795, "precision": 1e-6
        });
        assert_eq!(
            format_coordinate(&seconds),
            "39°49'41.430\"N, 98°34'46.200\"W"
        );
    }

    #[test]
    fn wikitext_is_escaped_but_a_url_is_not() {
        let escaped = r#"{"claims":{"P1":[{"rank":"normal","mainsnak":{"snaktype":"value","datatype":"string","datavalue":{"type":"string","value":"a&b|c"}}}]}}"#;
        assert_eq!(
            render_property(escaped, "P1", &HashMap::new()),
            "a&amp;b&#124;c"
        );
    }
}
