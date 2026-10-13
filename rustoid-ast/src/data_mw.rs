//! Textual `data-mw` JSON surgery.
//!
//! `data-mw` is compared byte-for-byte, so key order matters and a round trip
//! through a `serde_json::Value` (which sorts keys) would reorder it. These
//! helpers splice and strip object members textually instead.

/// PHP `DOMRangeBuilder::encapsulateTemplates` sets `$encapDataMw->parts = $parts`
/// on the encapsulation target: the transclusion `parts` are *appended* to the
/// target's own `data-mw`, keeping its keys (`name`/`attrs`/`body`) and their
/// order. A `<templatestyles>` sheet that is itself the target — a `<style>`
/// whose next sibling shares the transclusion's `about` — is served this way:
/// `{"name":"templatestyles","attrs":{…},"body":{…},"parts":[…]}`.
pub fn append_parts_to_data_mw(parts: Option<String>, target: Option<String>) -> Option<String> {
    let parts = parts?;
    let Some(target) = target else {
        return Some(parts);
    };
    // `parts` is a whole `{"parts":[…]}` envelope; its members go last inside
    // the target's object. Trimmed textually so the byte order survives (a
    // `serde_json::Value` round trip would sort the keys).
    let extra = parts
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim();
    if extra.is_empty() {
        return Some(target);
    }
    Some(splice_into_object(&target, extra))
}

/// The value of `key` in a flat JSON object, or `None`. Used to pull `parts`
/// out of a `data-mw` envelope without reparsing and losing key order.
pub fn data_mw_key_span(json: &str, key: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let lead = json[start..].len() - rest.len();
    let start = start + lead;
    let bytes = json.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in json[start..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                if depth == 0 {
                    return Some((start, start + i));
                }
                depth -= 1;
            }
            ',' if depth == 0 => return Some((start, start + i)),
            _ => {}
        }
        // A bare scalar ends at the comma or the closing brace.
        if depth == 0 && bytes.get(start + i).is_some_and(|b| *b == b',') {
            return Some((start, start + i));
        }
    }
    Some((start, json.len()))
}

/// `json` with the whole `"key":value` entry removed, leaving the other
/// entries intact.
///
/// Removing the *value alone* is not enough, and getting that wrong is silent:
/// it leaves `"key":` with nothing after it, so the envelope becomes
/// `{"parts":}` — which is what `merge_encap_data_mw` used to splice into the
/// output. That is not valid JSON, so the attribute it lands in cannot be read
/// by anything, and it reached the served HTML 131 times on one page.
///
/// The entry's own separator goes with it, and it is the *preceding* comma when
/// there is one, because removing the first entry of an object leaves a leading
/// comma otherwise. A key at the end of the object has no following comma to
/// take, so that case is handled by the preceding one too.
pub fn strip_data_mw_key(json: &str, key: &str) -> String {
    let Some((value_start, value_end)) = data_mw_key_span(json, key) else {
        return json.to_string();
    };
    // Extend the cut back over the key's `"key":` — including the quotes — so
    // the key goes with the value.
    let quoted = format!("\"{key}\":");
    let Some(key_start) = json[..value_start].rfind(&quoted) else {
        return json.to_string();
    };
    let mut from = key_start;
    let mut to = value_end;
    // Take the preceding comma when there is one, else the following one, so the
    // result is still a well-formed object either way.
    match json[..from].rfind(',') {
        Some(prev) => from = prev,
        None => {
            if json[to..].starts_with(',') {
                to += 1;
            }
        }
    }
    format!("{}{}", &json[..from], &json[to..])
}

/// Insert `extra` (already a `"key":value` fragment) into a `{…}` object.
pub fn splice_into_object(json: &str, extra: &str) -> String {
    match json.rfind('}') {
        Some(close) => {
            let head = json[..close].trim_end().trim_end_matches(',');
            // An empty object needs no separator.
            let sep = if head.trim_end().ends_with('{') {
                ""
            } else {
                ","
            };
            format!("{head}{sep}{extra}{}", &json[close..])
        }
        None => json.to_string(),
    }
}
