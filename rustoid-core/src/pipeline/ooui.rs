//! A minimal port of OOUI's PHP serializer.
//!
//! Extensions such as Phonos build their output with OOUI widgets, and the wiki
//! serves exactly what `OOUI\\Element::toString` produces. Reproducing that
//! byte-for-byte needs three things this module models:
//!
//! * the CSS-class vocabulary and its *order* (`Widget`/`ButtonElement`/
//!   `IconElement`/… each append their own classes as they initialise), the
//!   theme contributing none for the states the corpus exercises;
//! * the `data-ooui` JSON — `Element::getSerializedConfig` builds it by walking
//!   the config callbacks in registration order, so the key order is observable
//!   — written by a PHP-`json_encode`-compatible encoder (which escapes `/` as
//!   `\/` and non-ASCII as `\uXXXX`);
//! * the `ooui-php-N` ids OOUI hands out to infusable elements, from a counter
//!   that starts at 1 for each render.
//!
//! Only the pieces the corpus's extensions use are ported; anything else is
//! added when a served byte demands it.

use std::cell::Cell;

use crate::dom::node::{ElementKind, Node};

/// The `ooui-php-N` id counter. OOUI keeps a process-global counter and resets
/// it per request; rustoid renders one page per parser, so the counter lives
/// with the render and starts at 1.
#[derive(Debug, Default)]
pub struct OouiCounter {
    next: Cell<usize>,
}

impl OouiCounter {
    pub fn new() -> Self {
        Self { next: Cell::new(0) }
    }

    /// `Tag::generateElementId`: increment, then `ooui-php-<n>`.
    fn generate_id(&self) -> String {
        let n = self.next.get() + 1;
        self.next.set(n);
        format!("ooui-php-{n}")
    }
}

/// A JSON value with *ordered* object keys, because OOUI's `data-ooui` output
/// exposes the insertion order and the byte comparison sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Str(String),
    Bool(bool),
    Int(i64),
    Null,
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Encode as PHP `json_encode` does by default: escape `"` and `\`, escape
    /// `/` as `\/`, and escape non-ASCII as `\uXXXX` (no `JSON_UNESCAPED_*`).
    pub fn php_encode(&self) -> String {
        let mut out = String::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut String) {
        match self {
            Json::Str(s) => encode_php_string(s, out),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Null => out.push_str("null"),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.encode_into(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                // `json_encode` writes `{}` for an empty object; OOUI never
                // emits an empty one here, but be faithful anyway.
                if pairs.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode_php_string(key, out);
                    out.push(':');
                    value.encode_into(out);
                }
                out.push('}');
            }
        }
    }
}

/// Encode one JSON string body, PHP style.
fn encode_php_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                // PHP escapes non-ASCII as UTF-16 code units (`\uXXXX`), using a
                // surrogate pair above the BMP.
                let code = c as u32;
                if code <= 0xffff {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    let v = code - 0x10000;
                    let hi = 0xd800 + (v >> 10);
                    let lo = 0xdc00 + (v & 0x3ff);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Configuration for a `ButtonWidget`, mirroring OOUI's config keys.
#[derive(Debug, Clone, Default)]
pub struct ButtonWidget {
    /// `$config['href']` — set on the inner `<a>`.
    pub href: Option<String>,
    /// `$config['rel']` — joined with spaces on the inner `<a>`; OOUI defaults
    /// to `['nofollow']`.
    pub rel: Vec<String>,
    /// `$config['framed']` — default true; Phonos forces false.
    pub framed: bool,
    /// `$config['icon']` — symbolic icon name.
    pub icon: Option<String>,
    /// `$config['label']` — the label's DOM (a `HtmlSnippet`), empty when
    /// absent. Stored as a node so its children can be emitted inside the label
    /// span and serialized for `data-ooui` in one representation.
    pub label: Option<Node>,
    /// `$config['data']` — element data, serialized into `data-ooui`.
    pub data: Option<Json>,
    /// `$config['classes']` — the caller's own classes (OOUI's own are added).
    pub classes: Vec<String>,
    /// `$config['aria-label']` — Phonos sets it on the inner `<a>` as
    /// `aria-label` and `title`.
    pub aria_label: Option<String>,
    /// `$config['title']` — the `TitledElement` title.
    pub title: Option<String>,
    /// `$config['disabled']`.
    pub disabled: bool,
    /// Whether the widget's JS class is `mw.Phonos.PhonosButton` (which also
    /// forces `framed = false`, the `volumeUp` icon and `infusable`).
    pub phonos_button: bool,
    /// Extra class for an error state (`ext-phonos-error`).
    pub error: bool,
}

impl ButtonWidget {
    /// Build the widget as a DOM node, matching `OOUI\\ButtonWidget::toString`.
    ///
    /// The caller is responsible for the `PhonosButton`-specific config
    /// (frameless, `volumeUp`, infusable, `data-nosnippet`) via the fields above.
    pub fn into_node(self, counter: &OouiCounter) -> Node {
        let mut widget = self;
        if widget.phonos_button {
            // `PhonosButton::__construct`: `ext-phonos-PhonosButton` is pushed
            // first, then the error class, then the emptylabel class.
            widget.framed = false;
            if widget.icon.is_none() {
                widget.icon = Some("volumeUp".to_string());
            }
            widget.classes.push("ext-phonos-PhonosButton".to_string());
            if widget.error {
                widget.classes.push("ext-phonos-error".to_string());
                widget.icon = Some("volumeOff".to_string());
            }
            if widget.label.is_none() {
                widget
                    .classes
                    .push("ext-phonos-PhonosButton-emptylabel".to_string());
            }
        }

        // The root classes, in the order OOUI appends them.
        let mut root_classes = widget.classes.clone();
        root_classes.push("oo-ui-widget".to_string());
        root_classes.push(if widget.disabled {
            "oo-ui-widget-disabled".to_string()
        } else {
            "oo-ui-widget-enabled".to_string()
        });
        root_classes.push("oo-ui-buttonElement".to_string());
        root_classes.push(if widget.framed {
            "oo-ui-buttonElement-framed".to_string()
        } else {
            "oo-ui-buttonElement-frameless".to_string()
        });
        // A frameless button supports only `medium`, and that is the default
        // anyway, so the size class is always `medium`.
        root_classes.push("oo-ui-buttonElement-size-medium".to_string());
        if widget.icon.is_some() {
            root_classes.push("oo-ui-iconElement".to_string());
        }
        if widget.label.is_some() {
            root_classes.push("oo-ui-labelElement".to_string());
        }
        // (No indicator is used by the corpus's widgets.)
        root_classes.push("oo-ui-buttonWidget".to_string());

        // The inner `<a>`, with attributes in the order they are set.
        let mut a = Node::element(ElementKind::Other("a".to_string()));
        // `ButtonElement::initializeButtonElement`.
        a.set_attr("role", "button");
        // `TabIndexedElement::initializeTabIndexedElement` (default 0).
        a.set_attr("tabindex", "0");
        if let Some(href) = &widget.href {
            a.set_attr("href", href.clone());
        }
        if !widget.rel.is_empty() {
            a.set_attr("rel", widget.rel.join(" "));
        }
        if let Some(label) = &widget.aria_label {
            a.set_attr("aria-label", label.clone());
            a.set_attr("title", label.clone());
        } else if let Some(title) = &widget.title {
            a.set_attr("title", title.clone());
        }
        a.set_attr("class", "oo-ui-buttonElement-button".to_string());

        // The icon, label and indicator spans, in that content order. OOUI
        // always appends all three (`$this->button->appendContent($this->icon,
        // $this->label, $this->indicator)`), so each span is present even when
        // its content is empty.
        {
            let mut span = Node::element(ElementKind::Other("span".to_string()));
            match &widget.icon {
                Some(icon) => {
                    span.set_attr("class", format!("oo-ui-iconElement-icon oo-ui-icon-{icon}"))
                }
                None => span.set_attr("class", "oo-ui-iconElement-icon oo-ui-iconElement-noIcon"),
            }
            a.push_child(span);
        }
        {
            let mut span = Node::element(ElementKind::Other("span".to_string()));
            span.set_attr("class", "oo-ui-labelElement-label".to_string());
            if let Some(label) = &widget.label {
                for child in &label.children {
                    span.push_child(child.clone());
                }
            }
            a.push_child(span);
        }
        {
            // The indicator span is always present; with no indicator it carries
            // `oo-ui-indicatorElement-noIndicator` (and no root `oo-ui-indicatorElement`).
            let mut span = Node::element(ElementKind::Other("span".to_string()));
            span.set_attr(
                "class",
                "oo-ui-indicatorElement-indicator oo-ui-indicatorElement-noIndicator",
            );
            a.push_child(span);
        }

        // The root `<span>`, with its attributes in the order OOUI sets them:
        // the element's own attributes, then `id` (from `ensureInfusableId`),
        // then `class`, then `data-ooui` (from `getGeneratedAttributes`).
        let mut root = Node::element(ElementKind::Other("span".to_string()));
        if widget.phonos_button {
            root.set_attr("data-nosnippet", "");
        }
        // `PhonosButton` is always infusable, so it always takes an id.
        let id = counter.generate_id();
        root.set_attr("id", id);
        root.set_attr("class", root_classes.join(" "));
        root.set_attr("data-ooui", data_ooui(&widget));
        root.push_child(a);
        root
    }
}

/// `Element::getSerializedConfig` for a `ButtonWidget`/`PhonosButton`, in the
/// exact key order the config callbacks produce.
fn data_ooui(widget: &ButtonWidget) -> String {
    let mut config: Vec<(String, Json)> = Vec::new();
    // A placeholder for `_`, first as in `getSerializedConfig`.
    config.push(("_".to_string(), Json::Str(String::new())));

    // `ButtonWidget::getConfig` adds these *before* calling the parent.
    if let Some(href) = &widget.href {
        config.push(("href".to_string(), Json::Str(href.clone())));
    }
    if !widget.rel.is_empty() {
        config.push((
            "rel".to_string(),
            Json::Array(widget.rel.iter().cloned().map(Json::Str).collect()),
        ));
    }

    // The trait config callbacks, in `initialize*` order:
    // ButtonElement, IconElement, IndicatorElement, LabelElement, TitledElement,
    // FlaggedElement, TabIndexedElement, AccessKeyedElement.
    if !widget.framed {
        config.push(("framed".to_string(), Json::Bool(false)));
    }
    if let Some(icon) = &widget.icon {
        config.push(("icon".to_string(), Json::Str(icon.clone())));
    }
    // IndicatorElement: no indicator configured → nothing.
    // LabelElement: an `HtmlSnippet` serializes as `{ "html": "…" }`.
    if let Some(label) = &widget.label {
        config.push((
            "label".to_string(),
            Json::Object(vec![("html".to_string(), Json::Str(label_html(label)))]),
        ));
    }
    // TitledElement: the title is carried by Phonos as the aria-label, not as a
    // `title` config, so nothing.

    // `Element::getConfig` adds `data` then `classes` last.
    if let Some(data) = &widget.data {
        config.push(("data".to_string(), data.clone()));
    }
    if !widget.classes.is_empty() {
        config.push((
            "classes".to_string(),
            Json::Array(widget.classes.iter().cloned().map(Json::Str).collect()),
        ));
    }

    // `_` is set last to the JS class name.
    let js = if widget.phonos_button {
        "mw.Phonos.PhonosButton"
    } else {
        "OO.ui.ButtonWidget"
    };
    config[0].1 = Json::Str(js.to_string());

    Json::Object(config).php_encode()
}

/// The inner HTML of a label node, as `HtmlSnippet` carries it into `data-ooui`.
fn label_html(node: &Node) -> String {
    crate::html::serialize::HtmlSerializer::new(crate::options::ParserOptions::for_page(""))
        .serialize_children_to_string(node)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_json_escapes_slashes_and_quotes() {
        let v = Json::Object(vec![
            ("_".into(), Json::Str("mw.Phonos.PhonosButton".into())),
            (
                "href".into(),
                Json::Str("//upload.wikimedia.org/a.mp3".into()),
            ),
            (
                "rel".into(),
                Json::Array(vec![Json::Str("nofollow".into())]),
            ),
            ("framed".into(), Json::Bool(false)),
        ]);
        assert_eq!(
            v.php_encode(),
            r#"{"_":"mw.Phonos.PhonosButton","href":"\/\/upload.wikimedia.org\/a.mp3","rel":["nofollow"],"framed":false}"#
        );
    }

    #[test]
    fn php_json_escapes_non_ascii() {
        assert_eq!(Json::Str("ⓘ".into()).php_encode(), r#""\u24d8""#);
    }

    #[test]
    fn ooui_counter_starts_at_one() {
        let c = OouiCounter::new();
        assert_eq!(c.generate_id(), "ooui-php-1");
        assert_eq!(c.generate_id(), "ooui-php-2");
    }
}
