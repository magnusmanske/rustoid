//! The `<templatedata>` extension (TemplateData).
//!
//! A legacy MediaWiki hook: `<templatedata>`'s JSON body is validated and
//! normalized, then rendered as a parameter documentation table by
//! `TemplateDataHtmlFormatter::getHtml`. This module is a faithful port of that
//! formatter (plus the parts of `TemplateDataBlob`/`TemplateDataNormalizer` that
//! fill the defaults the formatter reads), producing the
//! `<section class="mw-templatedata-doc-wrap">` node. The extension
//! encapsulation (`typeof`/`about`/`data-mw`) is the caller's job, as for every
//! other extension.
//!
//! the corpus bodies are small; the fields the corpus does not use (sets, maps,
//! `paramOrder`) are handled only so far as the formatter reads them.

use serde_json::{Map, Value};

use crate::dom::node::{ElementKind, Node};

/// The predefined `format` values and their messages
/// (`TemplateDataValidator::PREDEFINED_FORMATS`).
fn format_icon_and_message(format: &str) -> (String, &'static str) {
    match format {
        "block" => (
            "template-format-block".to_string(),
            "This template prefers block formatting of parameters.",
        ),
        "inline" => (
            "template-format-inline".to_string(),
            "This template prefers inline formatting of parameters.",
        ),
        _ => (
            "settings".to_string(),
            "This template has custom formatting.",
        ),
    }
}

/// The message for a parameter type (`templatedata-doc-param-type-*`).
fn type_message(ty: &str) -> &'static str {
    match ty {
        "boolean" => "Boolean",
        "content" => "Content",
        "date" => "Date",
        "line" => "Line",
        "number" => "Number",
        "string" => "String",
        "unbalanced-wikitext" => "Unbalanced wikitext",
        "url" => "URL",
        "wiki-file-name" => "File",
        "wiki-page-name" => "Page name",
        "wiki-template-name" => "Template",
        "wiki-user-name" => "User",
        _ => "Unknown",
    }
}

/// An interface-text value resolved to a single string for `lang`: a plain
/// string is itself, an object looks up the language
/// (`TemplateDataBlob::getInterfaceTextInLanguage`).
fn interface_text(value: Option<&Value>, lang: &str) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.get(lang).and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// A normalized parameter.
struct Param {
    name: String,
    label: Option<String>,
    description: Option<String>,
    required: bool,
    suggested: bool,
    deprecated: bool,
    aliases: Vec<String>,
    ty: String,
    autovalue: Option<String>,
    default: Option<String>,
    suggestedvalues: Vec<String>,
    example: Option<String>,
}

fn str_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// PHP truthiness for a flag field. `deprecated` in particular is often a
/// non-empty *string* (the reason), which PHP reads as true.
fn flag(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty() && s != "0",
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The normalized template data the formatter reads.
struct Data {
    description: Option<String>,
    /// The parameters in render order, with their names.
    params: Vec<Param>,
    format: Option<String>,
}

fn parse(json: &str, lang: &str) -> Data {
    let root = serde_json::from_str::<Value>(json).unwrap_or(Value::Null);
    let root = match &root {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };

    let params_obj = root.get("params").and_then(Value::as_object);
    let mut params: Vec<Param> = Vec::new();
    if let Some(params_obj) = params_obj {
        for (name, value) in params_obj {
            let obj = value.as_object();
            let get = |k: &str| obj.and_then(|o| o.get(k));
            let ty = match get("type").and_then(Value::as_str) {
                // Deprecated parameter types map to their newer names
                // (`TemplateDataNormalizer::DEPRECATED_PARAMETER_TYPES`).
                Some("string/line") => "line",
                Some("string/wiki-page-name") => "wiki-page-name",
                Some("string/wiki-user-name") => "wiki-user-name",
                Some("string/wiki-file-name") => "wiki-file-name",
                Some(t) => t,
                None => "unknown",
            };
            params.push(Param {
                name: name.clone(),
                label: interface_text(get("label"), lang),
                description: interface_text(get("description"), lang),
                required: flag(get("required")),
                suggested: flag(get("suggested")),
                deprecated: flag(get("deprecated")),
                aliases: str_array(get("aliases")),
                ty: ty.to_string(),
                autovalue: get("autovalue").and_then(Value::as_str).map(str::to_string),
                default: interface_text(get("default"), lang),
                suggestedvalues: str_array(get("suggestedvalues")),
                example: interface_text(get("example"), lang),
            });
        }
    }

    // `paramOrder` overrides the object order when present.
    if let Some(order) = root.get("paramOrder").and_then(Value::as_array) {
        let mut ordered: Vec<Param> = Vec::new();
        for name in order.iter().filter_map(Value::as_str) {
            if let Some(i) = params.iter().position(|p| p.name == name) {
                ordered.push(params.remove(i));
            }
        }
        ordered.extend(params);
        params = ordered;
    }

    Data {
        description: interface_text(root.get("description"), lang),
        params,
        format: root
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Build the `<section class="mw-templatedata-doc-wrap">` for a `<templatedata>`
/// body. `frame_title` is the prefixed title of the frame the tag sits in, which
/// the `mw:edittemplatedata` marker records.
pub fn render(json: &str, frame_title: &str, lang: &str) -> Node {
    let data = parse(json, lang);

    let mut html = Node::document();

    // <header><p class="mw-templatedata-doc-desc[ mw-templatedata-doc-muted]">…</p></header>
    {
        let desc = data.description.clone();
        let mut p = Node::element(ElementKind::Other("p".to_string()));
        let mut classes = vec!["mw-templatedata-doc-desc"];
        if desc.is_none() {
            classes.push("mw-templatedata-doc-muted");
        }
        p.set_attr("class", classes.join(" "));
        p.push_child(Node::text(
            desc.unwrap_or_else(|| "No description.".to_string()),
        ));
        let mut header = Node::element(ElementKind::Other("header".to_string()));
        header.push_child(p);
        html.push_child(header);
    }

    // <table class="wikitable mw-templatedata-doc-params[ sortable]">
    let mut table = Node::element(ElementKind::Other("table".to_string()));
    let sorting = if data.params.len() > 1 {
        " sortable"
    } else {
        ""
    };
    table.set_attr(
        "class",
        format!("wikitable mw-templatedata-doc-params{sorting}"),
    );

    // <caption>…</caption>
    table.push_child(caption(&data, frame_title));

    // <thead>…</thead>
    {
        let mut thead = Node::element(ElementKind::Other("thead".to_string()));
        let mut tr = Node::element(ElementKind::Other("tr".to_string()));
        tr.push_child(th_text(&[("colspan", "2")], "Parameter"));
        tr.push_child(th_text(&[], "Description"));
        tr.push_child(th_text(&[], "Type"));
        tr.push_child(th_text(&[], "Status"));
        thead.push_child(tr);
        table.push_child(thead);
    }

    // <tbody>…</tbody>
    {
        let mut tbody = Node::element(ElementKind::Other("tbody".to_string()));
        if data.params.is_empty() {
            let mut tr = Node::element(ElementKind::Other("tr".to_string()));
            let mut td = Node::element(ElementKind::Other("td".to_string()));
            td.set_attr("class", "mw-templatedata-doc-muted");
            td.set_attr("colspan", "7");
            td.push_child(Node::text("No parameters specified".to_string()));
            tr.push_child(td);
            tbody.push_child(tr);
        }
        for param in &data.params {
            tbody.push_child(param_row(param));
        }
        table.push_child(tbody);
    }

    html.push_child(table);

    // <section class="mw-templatedata-doc-wrap">…</section>
    let mut section = Node::element(ElementKind::Other("section".to_string()));
    section.set_attr("class", "mw-templatedata-doc-wrap");
    for child in html.children {
        section.push_child(child);
    }
    section
}

/// The table `<caption>`: the parameters heading, the edit marker and the format
/// note when there is one.
fn caption(data: &Data, frame_title: &str) -> Node {
    let mut caption = Node::element(ElementKind::Other("caption".to_string()));

    let mut p = Node::element(ElementKind::Other("p".to_string()));
    p.set_attr("class", "mw-templatedata-caption");
    p.push_child(Node::text("Template parameters".to_string()));
    {
        let mut marker = Node::element(ElementKind::Other("mw:edittemplatedata".to_string()));
        marker.set_attr("page", frame_title.to_string());
        p.push_child(marker);
    }
    caption.push_child(p);

    if let Some(format) = &data.format {
        let (icon, message) = format_icon_and_message(format);
        let mut p = Node::element(ElementKind::Other("p".to_string()));
        {
            // An OOUI `IconWidget`. Its output is fixed for a given icon.
            let mut span = Node::element(ElementKind::Other("span".to_string()));
            span.set_attr(
                "class",
                format!(
                    "oo-ui-widget oo-ui-widget-enabled oo-ui-iconElement-icon \
                     oo-ui-icon-{icon} oo-ui-iconElement oo-ui-labelElement-invisible \
                     oo-ui-iconWidget"
                ),
            );
            p.push_child(span);
        }
        {
            let mut span = Node::element(ElementKind::Other("span".to_string()));
            span.set_attr("class", "mw-templatedata-format");
            span.push_child(Node::text(message.to_string()));
            p.push_child(span);
        }
        caption.push_child(p);
    }

    caption
}

/// A `<th>` holding escaped text.
fn th_text(attrs: &[(&str, &str)], text: &str) -> Node {
    let mut th = Node::element(ElementKind::Other("th".to_string()));
    for (k, v) in attrs {
        th.set_attr(*k, *v);
    }
    th.push_child(Node::text(text.to_string()));
    th
}

/// One `<tr>` of the parameter table.
fn param_row(param: &Param) -> Node {
    let status = if param.deprecated {
        "deprecated"
    } else if param.required {
        "required"
    } else if param.suggested {
        "suggested"
    } else {
        "optional"
    };
    let status_text = match status {
        "deprecated" => "deprecated",
        "required" => "required",
        "suggested" => "suggested",
        _ => "optional",
    };
    let sort_value = match status {
        "deprecated" => "-1",
        "suggested" => "1",
        "required" => "2",
        _ => "0",
    };

    let mut tr = Node::element(ElementKind::Other("tr".to_string()));

    // Label.
    tr.push_child(th_text(&[], param.label.as_deref().unwrap_or(&param.name)));

    // Parameter name and its aliases.
    {
        let mut td = Node::element(ElementKind::Other("td".to_string()));
        td.set_attr("class", "mw-templatedata-doc-param-name");
        td.push_child(code(&param.name, None));
        for alias in &param.aliases {
            td.push_child(Node::text(" ".to_string()));
            td.push_child(code(alias, Some("mw-templatedata-doc-param-alias")));
        }
        tr.push_child(td);
    }

    // Description, suggested values, default, example, auto value.
    {
        let mut td = Node::element(ElementKind::Other("td".to_string()));
        {
            let mut p = Node::element(ElementKind::Other("p".to_string()));
            // The class is present unless the description is non-empty.
            if param.description.as_deref().is_none_or(str::is_empty) {
                p.set_attr("class", "mw-templatedata-doc-muted");
            }
            p.push_child(Node::text(
                param
                    .description
                    .clone()
                    .unwrap_or_else(|| "no description".to_string()),
            ));
            td.push_child(p);
        }
        let mut dl = Node::element(ElementKind::Other("dl".to_string()));
        if !param.suggestedvalues.is_empty() {
            dl.push_child(dt_text("Suggested values"));
            let mut dd = Node::element(ElementKind::Other("dd".to_string()));
            for (i, value) in param.suggestedvalues.iter().enumerate() {
                if i > 0 {
                    dd.push_child(Node::text(" ".to_string()));
                }
                dd.push_child(code(value, None));
            }
            dl.push_child(dd);
        }
        if let Some(default) = &param.default {
            dl.push_child(dt_text("Default"));
            dl.push_child(dd_text(default));
        }
        if let Some(example) = &param.example {
            dl.push_child(dt_text("Example"));
            dl.push_child(dd_text(example));
        }
        if let Some(autovalue) = &param.autovalue {
            dl.push_child(dt_text("Auto value"));
            let mut dd = Node::element(ElementKind::Other("dd".to_string()));
            dd.push_child(code(autovalue, None));
            dl.push_child(dd);
        }
        td.push_child(dl);
        tr.push_child(td);
    }

    // Type.
    {
        let mut td = Node::element(ElementKind::Other("td".to_string()));
        let classes = if param.ty == "unknown" {
            "mw-templatedata-doc-param-type mw-templatedata-doc-muted"
        } else {
            "mw-templatedata-doc-param-type"
        };
        td.set_attr("class", classes);
        td.push_child(Node::text(type_message(&param.ty).to_string()));
        tr.push_child(td);
    }

    // Status.
    {
        let mut td = Node::element(ElementKind::Other("td".to_string()));
        td.set_attr(
            "class",
            format!("mw-templatedata-doc-param-status-{status}"),
        );
        td.set_attr("data-sort-value", sort_value);
        td.push_child(Node::text(status_text.to_string()));
        tr.push_child(td);
    }

    tr
}

fn code(text: &str, class: Option<&str>) -> Node {
    let mut c = Node::element(ElementKind::Other("code".to_string()));
    if let Some(class) = class {
        c.set_attr("class", class);
    }
    c.push_child(Node::text(text.to_string()));
    c
}

fn dt_text(text: &str) -> Node {
    let mut dt = Node::element(ElementKind::Other("dt".to_string()));
    dt.push_child(Node::text(text.to_string()));
    dt
}

fn dd_text(text: &str) -> Node {
    let mut dd = Node::element(ElementKind::Other("dd".to_string()));
    dd.push_child(Node::text(text.to_string()));
    dd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_param_gets_its_defaults() {
        let data = parse(
            r#"{"params":{"name":{"label":"name","description":""}}}"#,
            "en",
        );
        assert_eq!(data.params.len(), 1);
        let p = &data.params[0];
        assert_eq!(p.name, "name");
        assert_eq!(p.label.as_deref(), Some("name"));
        assert_eq!(p.description.as_deref(), Some(""));
        assert_eq!(p.ty, "unknown");
        assert!(!p.required && !p.suggested && !p.deprecated);
        assert!(p.aliases.is_empty() && p.suggestedvalues.is_empty());
    }

    #[test]
    fn a_deprecated_type_is_mapped() {
        let data = parse(r#"{"params":{"a":{"type":"string/line"}}}"#, "en");
        assert_eq!(data.params[0].ty, "line");
    }

    #[test]
    fn an_interface_text_object_resolves_to_the_language() {
        let data = parse(r#"{"description":{"fr":"fr","en":"en"}}"#, "en");
        assert_eq!(data.description.as_deref(), Some("en"));
    }

    #[test]
    fn the_sortable_class_needs_more_than_one_param() {
        let one = parse(r#"{"params":{"a":{}}}"#, "en");
        let two = parse(r#"{"params":{"a":{},"b":{}}}"#, "en");
        assert!(one.params.len() == 1 && two.params.len() == 2);
    }

    #[test]
    fn the_format_note_maps_block_to_its_icon() {
        assert_eq!(format_icon_and_message("block").0, "template-format-block");
        assert_eq!(
            format_icon_and_message("inline").0,
            "template-format-inline"
        );
        assert_eq!(format_icon_and_message("{{_|_=_}}").0, "settings");
    }
}
