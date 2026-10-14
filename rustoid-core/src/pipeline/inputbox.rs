//! The `<inputbox>` extension (InputBox).
//!
//! A legacy MediaWiki hook: the tag body is a `key=value` option list, and
//! `InputBox::render` builds one of several forms. Only the search forms
//! (`type=search` / `type=fulltext`, the corpus's case) are ported here; the
//! edit forms (`create`/`comment`/`move`) are left to the generic `<extension>`
//! fallback until a page needs them.
//!
//! The HTML is `InputBox::getSearchForm` with its helpers (`buildTextBox`,
//! `buildCheckboxInput`, `buildSubmitInput`, `buildSearchActionUrl`), rendered as
//! DOM nodes so the extension encapsulation (`typeof`/`about`/`data-mw`) can be
//! added by the caller as for every other extension.

use std::collections::HashMap;

use crate::dom::node::{ElementKind, Node};
use crate::traits::SiteConfig;

/// The message values `getSearchForm` falls back to (English).
const TRYEXACT: &str = "Go";
const SEARCHFULLTEXT: &str = "Search";

/// The parsed `<inputbox>` options, with `extractOptions`'s defaults applied.
#[derive(Debug, Default)]
pub struct Options {
    pub type_: String,
    width: i32,
    default_text: String,
    placeholder: String,
    bg_color: String,
    button_label: String,
    search_button_label: String,
    namespaces: String,
    hidden: String,
    id: String,
    dir: String,
    search_engine: String,
    search_type: String,
    search_filter: String,
    tour: String,
    /// The line break between fields: `' '` for `break=no`, else `<br />`.
    br: String,
    prefix: String,
    textbox_aria_label: String,
}

impl Options {
    /// `extractOptions`: parse the `key=value` lines, then apply the defaults and
    /// the small validations.
    pub fn parse(body: &str) -> Self {
        let mut values: HashMap<String, String> = HashMap::new();
        for line in body.split('\n') {
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim().to_lowercase();
            let value = crate::html::wts_utils::decode_wt_entities_all(value.trim());
            values.insert(name, value);
        }
        if let Some(dir) = values.get("dir")
            && dir != "ltr"
            && dir != "rtl"
        {
            values.remove("dir");
        }

        let get = |k: &str| values.get(k).cloned().unwrap_or_default();
        let mut opts = Options {
            type_: get("type"),
            width: get("width").parse::<i32>().unwrap_or(0),
            default_text: get("default"),
            placeholder: get("placeholder"),
            bg_color: get("bgcolor"),
            button_label: get("buttonlabel"),
            search_button_label: get("searchbuttonlabel"),
            namespaces: get("namespaces"),
            hidden: get("hidden"),
            id: get("id"),
            dir: get("dir"),
            search_engine: get("searchengine"),
            search_type: get("searchtype"),
            search_filter: get("searchfilter"),
            tour: get("tour"),
            br: get("break"),
            prefix: get("prefix"),
            textbox_aria_label: get("arialabel"),
        };

        // `mBR = strtolower(mBR) === 'no' ? ' ' : '<br />'`.
        opts.br = if opts.br.to_lowercase() == "no" {
            " ".to_string()
        } else {
            "<br />".to_string()
        };
        // `mWidth = intval(mWidth <= 0 ? 50 : mWidth)`.
        if opts.width <= 0 {
            opts.width = 50;
        }
        if !is_valid_color(&opts.bg_color) {
            opts.bg_color = "transparent".to_string();
        }
        if opts.dir.is_empty() {
            opts.dir = "ltr".to_string();
        }
        opts
    }

    /// `InputBox::render` for the search types, returning `None` for a type this
    /// port does not handle (the caller then leaves the tag visible).
    pub fn render(&self, config: &dyn SiteConfig, rand_suffix: &str) -> Option<Node> {
        match self.type_.as_str() {
            "search" => Some(self.search_form("search", config, rand_suffix)),
            "fulltext" => Some(self.search_form("fulltext", config, rand_suffix)),
            _ => None,
        }
    }

    /// `getFormLinebreakClasses` + `getSearchFormClasses`.
    fn search_form_classes(&self) -> String {
        let break_class = if self.br == "<br />" {
            " mw-inputbox-form"
        } else {
            " mw-inputbox-form-inline"
        };
        let engine = if self.search_engine.is_empty() {
            " inputbox-searchengine-not-set"
        } else {
            " inputbox-searchengine-set"
        };
        format!("{break_class}{engine}")
    }

    /// `getLinebreakClasses`.
    fn linebreak_classes(&self) -> &'static str {
        if self.br == "<br />" {
            "mw-inputbox-input "
        } else {
            ""
        }
    }

    /// `bgColorStyle`.
    fn bg_style(&self) -> String {
        if self.bg_color != "transparent" {
            format!("background-color: {}; color: inherit;", self.bg_color)
        } else {
            String::new()
        }
    }

    /// `getSearchForm($type)`.
    fn search_form(&self, type_: &str, config: &dyn SiteConfig, rand_suffix: &str) -> Node {
        let button_label = if self.button_label.is_empty() {
            TRYEXACT
        } else {
            &self.button_label
        };
        let search_button_label = if self.search_button_label.is_empty() {
            SEARCHFULLTEXT
        } else {
            &self.search_button_label
        };
        let id_rand = format!("-{}{}", self.id, rand_suffix);

        let mut centered = Node::element(ElementKind::Other("div".to_string()));
        centered.set_attr("class", "mw-inputbox-centered");
        centered.set_attr("style", self.bg_style());

        let mut form = Node::element(ElementKind::Other("form".to_string()));
        form.set_attr("name", "searchbox");
        form.set_attr("class", format!("searchbox{}", self.search_form_classes()));
        form.set_attr("action", search_action_url(config));
        if !self.id.is_empty() {
            form.set_attr("id", self.id.clone());
        }

        // The text box (`buildTextBox`).
        {
            let mut wrap = Node::element(ElementKind::Other("div".to_string()));
            wrap.set_attr("class", "cdx-text-input");
            let mut input = Node::element(ElementKind::Other("input".to_string()));
            input.set_attr(
                "class",
                format!(
                    "{}mw-searchInput searchboxInput cdx-text-input__input",
                    self.linebreak_classes()
                ),
            );
            input.set_attr("name", "search");
            // `type=text` and an empty `value` are absent from the served HTML.
            if !self.hidden.is_empty() {
                input.set_attr("type", "hidden");
            }
            if !self.default_text.is_empty() {
                input.set_attr("value", self.default_text.clone());
            }
            input.set_attr("placeholder", self.placeholder.clone());
            input.set_attr("size", self.width.to_string());
            input.set_attr("dir", self.dir.clone());
            if !self.textbox_aria_label.is_empty() {
                input.set_attr("aria-label", self.textbox_aria_label.clone());
            }
            wrap.push_child(input);
            form.push_child(wrap);
        }

        if !self.prefix.is_empty() {
            form.push_child(hidden_input("prefix", &self.prefix));
        }
        if !self.search_filter.is_empty() {
            form.push_child(hidden_input("searchfilter", &self.search_filter));
        }
        if !self.search_type.is_empty() && self.search_engine == "MediaSearch" {
            form.push_child(hidden_input("type", &self.search_type));
        }
        if !self.tour.is_empty() {
            form.push_child(hidden_input("tour", &self.tour));
        }

        form.push_child(Node::text(self.br.clone()));

        // Namespace checkboxes.
        if !self.namespaces.is_empty() {
            let show = self.valid_namespaces(config);
            for (id, name) in &show {
                let checked = self.namespace_checked(*id, name, &show);
                if show.len() == 1 {
                    let mut input = Node::element(ElementKind::Other("input".to_string()));
                    input.set_attr("type", "hidden");
                    input.set_attr("name", format!("ns{id}"));
                    input.set_attr("value", "1");
                    input.set_attr("id", format!("mw-inputbox-ns{id}{id_rand}"));
                    if checked {
                        input.set_attr("checked", "");
                    }
                    form.push_child(input);
                } else {
                    // `buildCheckboxInput` emits `' <span…>…</span> '`, spaces included.
                    form.push_child(Node::text(" ".to_string()));
                    form.push_child(checkbox_input(
                        name,
                        &format!("ns{id}"),
                        &format!("mw-inputbox-ns{id}{id_rand}"),
                        "1",
                        checked,
                    ));
                    form.push_child(Node::text(" ".to_string()));
                }
            }
            form.push_child(Node::text(self.br.clone()));
        } else if type_ == "search" {
            form.push_child(submit_input("go", button_label));
            form.push_child(Node::text("\u{a0}".to_string()));
        }

        // Search button.
        form.push_child(submit_input("fulltext", search_button_label));
        if type_ == "fulltext" {
            form.push_child(hidden_input("fulltext", "Search"));
        }

        centered.push_child(form);
        centered
    }

    /// The valid `namespaces=` entries, in source order, as `(id, name)`.
    fn valid_namespaces(&self, config: &dyn SiteConfig) -> Vec<(i32, String)> {
        let mut out: Vec<(i32, String)> = Vec::new();
        for user_ns in self.namespaces.split(',') {
            let mut name = user_ns.trim().to_string();
            if name.contains("**") {
                name = name.replace("**", "");
            }
            let id = if name == "Main" {
                Some(0)
            } else {
                config.namespace_id(&name)
            };
            if let Some(id) = id
                && !out.iter().any(|(i, _)| *i == id)
            {
                out.push((id, name));
            }
        }
        out
    }

    /// Whether a namespace checkbox is checked: flagged `**` or the only one.
    fn namespace_checked(&self, id: i32, name: &str, show: &[(i32, String)]) -> bool {
        if show.len() == 1 {
            return true;
        }
        self.namespaces
            .split(',')
            .any(|ns| ns.trim() == format!("{name}**"))
            && show.iter().any(|(i, _)| *i == id)
    }
}

/// `buildTextBox`'s inner `<input>` is wrapped by the caller; these are the
/// shared input builders.
fn hidden_input(name: &str, value: &str) -> Node {
    let mut input = Node::element(ElementKind::Other("input".to_string()));
    input.set_attr("type", "hidden");
    input.set_attr("value", value.to_string());
    input.set_attr("name", name.to_string());
    input
}

/// `buildSubmitInput`.
fn submit_input(name: &str, value: &str) -> Node {
    let mut input = Node::element(ElementKind::Other("input".to_string()));
    input.set_attr("type", "submit");
    input.set_attr("name", name.to_string());
    input.set_attr("value", value.to_string());
    input.set_attr("class", "cdx-button");
    input
}

/// `buildCheckboxInput`: a Codex checkbox with its label.
fn checkbox_input(label: &str, name: &str, id: &str, value: &str, checked: bool) -> Node {
    let mut span = Node::element(ElementKind::Other("span".to_string()));
    span.set_attr("class", "cdx-checkbox cdx-checkbox--inline");
    {
        let mut input = Node::element(ElementKind::Other("input".to_string()));
        input.set_attr("type", "checkbox");
        input.set_attr("name", name.to_string());
        input.set_attr("value", value.to_string());
        input.set_attr("id", id.to_string());
        input.set_attr("class", "cdx-checkbox__input");
        if checked {
            input.set_attr("checked", "");
        }
        span.push_child(input);
    }
    {
        let mut icon = Node::element(ElementKind::Other("span".to_string()));
        icon.set_attr("class", "cdx-checkbox__icon");
        span.push_child(icon);
    }
    {
        let mut lab = Node::element(ElementKind::Other("label".to_string()));
        lab.set_attr("class", "cdx-checkbox__label");
        lab.set_attr("for", id.to_string());
        lab.push_child(Node::text(label.to_string()));
        span.push_child(lab);
    }
    span
}

/// `buildSearchActionUrl`: `Special:Search`'s local URL (MediaSearch is not
/// assumed installed).
fn search_action_url(config: &dyn SiteConfig) -> String {
    config.article_path().replace("$1", "Special:Search")
}

/// `isValidColor`: a colour name, `#rgb`, `#rrggbb` or `rgb(...)`.
fn is_valid_color(color: &str) -> bool {
    if color.is_empty() {
        return false;
    }
    if color.chars().all(|c| c.is_ascii_alphabetic()) {
        return true;
    }
    let hex = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(rest) = color.strip_prefix('#') {
        return hex(rest, 3) || hex(rest, 6);
    }
    if let Some(rest) = color
        .strip_prefix("rgb")
        .map(|r| r.trim_start())
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return false;
        }
        let int = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        let pct = |s: &str| {
            s.strip_suffix('%')
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit() || c == '.'))
        };
        return parts.iter().all(|p| int(p)) || parts.iter().all(|p| pct(p));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    #[test]
    fn options_parse_and_default() {
        let o = Options::parse(
            "\ntype=fulltext\nwidth=40\nnamespaces=Help**,Wikipedia**\n\
             searchbuttonlabel=Search\nbreak=no\ninline=yes\nplaceholder=Search help\n",
        );
        assert_eq!(o.type_, "fulltext");
        assert_eq!(o.width, 40);
        assert_eq!(o.namespaces, "Help**,Wikipedia**");
        assert_eq!(o.search_button_label, "Search");
        assert_eq!(o.br, " ");
        assert_eq!(o.bg_color, "transparent");
        assert_eq!(o.dir, "ltr");
    }

    #[test]
    fn an_unknown_type_is_not_rendered() {
        let o = Options::parse("type=create");
        let config = MockSiteConfig::default();
        assert!(o.render(&config, "-0.1").is_none());
    }

    #[test]
    fn a_bad_width_falls_back_to_fifty() {
        let o = Options::parse("type=search\nwidth=0");
        assert_eq!(o.width, 50);
    }

    #[test]
    fn colour_validation() {
        assert!(is_valid_color("red"));
        assert!(is_valid_color("#abc"));
        assert!(is_valid_color("#aabbcc"));
        assert!(is_valid_color("rgb(1, 2, 3)"));
        // The regex's name branch is `[a-zA-Z]*`, so any word passes.
        assert!(is_valid_color("javascript"));
        assert!(!is_valid_color(""));
        assert!(!is_valid_color("rgb(1, 2)"));
    }
}
