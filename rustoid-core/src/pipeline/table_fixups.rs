//! `TableFixups` — DOM traversal that fixes template-induced interrupted table
//! cell parsing by recombining cells and/or reparsing cell content as
//! attributes. A faithful port of PHP Parsoid's
//! `src/Wt2Html/DOM/Handlers/TableFixups.php`.
//!
//! Templated cells can straddle a template boundary: some content comes from
//! top-level source and some from a template, and Parsoid tokenizes template
//! content independent of the surrounding context. When a cell's attribute
//! box (`|…|` for `<td>` / `!…|` for `<th>`) is produced by a template
//! (`|{{table_attribs}}` expanding to `style="color:red;"|Foo`), the tokenizer
//! sees only the cell content; this pass re-interprets the leading `k=v|`
//! prefix as attributes and splits hidden `<td>`/`<th>` cells back out on the
//! embedded `||`/`!!` separators.

use crate::dom::node::{ElementKind, Node, NodeKind};
use crate::html::wts_utils::node_name;
use crate::traits::SiteConfig;
use crate::wikitext::tokens_v2::{DataParsoid, DomSourceRange};

/// The reparse scenarios from `getReparseType` / `pipeStatusInContent`.
/// Mirrors the PHP `ReparseScenario` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReparseScenario {
    /// No further processing needed.
    NotNeeded,
    /// Merge this cell with the previous one (`reparseWithPreviousCell`).
    MaybeCombineWithPrevCell,
    /// Reinterpret templated cell content as attributes.
    MaybeReparseAttrs,
    /// Split the cell on hidden `||`/`!!` separators.
    MaybeSplitCell,
}

fn name(node: &Node) -> String {
    node_name(node)
}

/// `DOMUtils::hasTypeOf` — literal membership of `ty` in the `typeof` attribute.
fn has_type_of(node: &Node, ty: &str) -> bool {
    crate::html::dom_utils::has_type_of(node, ty)
}

/// `WTUtils::WIKILINK_SYNTAX_CONSTRUCTS_REGEXP`
/// (`#^mw:(WikiLink(/Interwiki)?|MediaLink|PageProp/(Category|Language))$#`).
fn wikilink_syntax_construct_regexp(rel: &str) -> bool {
    matches!(
        rel,
        "mw:WikiLink"
            | "mw:WikiLink/Interwiki"
            | "mw:MediaLink"
            | "mw:PageProp/Category"
            | "mw:PageProp/Language"
    )
}

/// `shouldAbortAttr` — the legacy parser aborts attribute parsing on wikilinks
/// and figure/language-converter constructs; those must also stop collection.
fn should_abort_attr(child: &Node) -> bool {
    child
        .get_attr("rel")
        .is_some_and(|r| r.split_whitespace().any(wikilink_syntax_construct_regexp))
        || crate::html::wts_utils::is_generated_figure(child)
}

/// `isSimpleTemplatedSpan` — a `<span>` with `about` and only text/comment children.
fn is_simple_templated_span(node: &Node) -> bool {
    name(node) == "span"
        && node.get_attr("about").is_some()
        && node
            .children
            .iter()
            .all(|c| matches!(c.kind, NodeKind::Text(_) | NodeKind::Comment(_)))
}

/// Recursively collect the text content of a node (mirrors `$child->textContent`).
fn text_content(node: &Node) -> String {
    let mut out = String::new();
    for child in &node.children {
        match &child.kind {
            NodeKind::Text(t) => out.push_str(t),
            _ => out.push_str(&text_content(child)),
        }
    }
    out
}

/// `/(?:^|[^|])\|(?:[^|]|$)/D` — does `text` contain a `|` not adjacent to another `|`?
fn pipe_in_text(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'|' {
            let prev_ok = i == 0 || bytes[i - 1] != b'|';
            let next_ok = i + 1 == bytes.len() || bytes[i + 1] != b'|';
            if prev_ok && next_ok {
                return true;
            }
        }
    }
    false
}

/// The attributed content collected from a cell: the accumulated text,
/// `nowiki` fragment payloads (for `<frag-marker>` splicing), and the
/// `mw:Transclusion` nodes encountered.
#[derive(Clone)]
struct AttributishContent {
    txt: String,
    frags: Vec<String>,
    transclusions: Vec<Node>,
}

/// `collectAttributishContent` — walk the cell's children accumulating the
/// "attributish" prefix (text / `mw:Entity` / `mw:Transclusion` /
/// `mw:DOMFragment`), stopping at a pipe character. Returns `None` when no
/// pipe is found (nothing reparseable).
fn collect_attributish_content(
    cell: &Node,
    template_wrapper: Option<Node>,
) -> Option<AttributishContent> {
    let mut buf = String::new();
    let mut frags: Vec<String> = Vec::new();
    let mut transclusions: Vec<Node> = Vec::new();

    if let Some(wrapper) = template_wrapper {
        transclusions.push(wrapper);
    }

    let mut found_stop = false;
    walk_attributish(
        &cell.children,
        &mut buf,
        &mut frags,
        &mut transclusions,
        &mut found_stop,
    );

    if found_stop {
        Some(AttributishContent {
            txt: buf,
            frags,
            transclusions,
        })
    } else {
        None
    }
}

fn walk_attributish(
    nodes: &[Node],
    buf: &mut String,
    frags: &mut Vec<String>,
    transclusions: &mut Vec<Node>,
    found_stop: &mut bool,
) {
    for child in nodes {
        match &child.kind {
            NodeKind::Comment(_) => {
                // Legacy parser strips comments during parsing => drop them.
            }
            NodeKind::Text(text) => {
                buf.push_str(text);
                if pipe_in_text(text) {
                    *found_stop = true;
                    return;
                }
            }
            NodeKind::Element(_) => {
                if has_type_of(child, "mw:Transclusion") {
                    transclusions.push(child.clone());
                }

                if has_type_of(child, "mw:Entity") {
                    // Get the entity's wikitext source, not rendered content
                    // (`&#10;` is `"\n"` which breaks attribute parsing!).
                    let src = child
                        .dp
                        .as_ref()
                        .and_then(|d| d.src.clone())
                        .unwrap_or_else(|| text_content(child));
                    buf.push_str(&src);
                } else if has_type_of(child, "mw:DOMFragment") {
                    // For nowikis the fragment's first child is the protected text.
                    let frag_text = child
                        .fragment
                        .as_ref()
                        .and_then(|f| f.children.first())
                        .map(text_content)
                        .unwrap_or_default();
                    frags.push(frag_text);
                    buf.push_str("<frag-marker>");
                } else if should_abort_attr(child) {
                    *found_stop = true;
                    return;
                } else {
                    walk_attributish(&child.children, buf, frags, transclusions, found_stop);
                    if *found_stop {
                        return;
                    }
                }
            }
            NodeKind::Document => {}
        }
    }
}

/// `/(^[^|]+\|)([^|]|$)/D` — return the captured group 1 (the `k=v|` prefix),
/// or `None` when the text has no usable `|` separator.
fn attributish_prefix(text: &str) -> Option<String> {
    let first_pipe = text.find('|')?;
    if first_pipe == 0 {
        return None;
    }
    let after = &text[first_pipe + 1..];
    if after.starts_with('|') {
        return None;
    }
    Some(text[..=first_pipe].to_string())
}

/// Collapse runs of whitespace to a single space (mirrors `preg_replace('/\s+/',' ')`).
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// `reparseTemplatedAttributes` — the heart of the `k=v|content` reparse.
/// Reparse a template-produced attribute prefix off the cell, applying the
/// attributes to the cell and dropping the consumed content.
fn reparse_templated_attributes(
    cell: &mut Node,
    template_wrapper: Option<Node>,
    config: &dyn SiteConfig,
    source: Option<&str>,
) {
    let Some(content) = collect_attributish_content(cell, template_wrapper) else {
        return;
    };

    let Some(mut attributish_prefix) = attributish_prefix(&content.txt) else {
        return;
    };

    // Splice fragment content back into the prefix (normalizing whitespace).
    if attributish_prefix.contains("<frag-marker>") {
        let mut frags = content.frags.iter();
        let mut out = String::new();
        let mut rest = attributish_prefix.as_str();
        while let Some(idx) = rest.find("<frag-marker>") {
            out.push_str(&rest[..idx]);
            if let Some(frag) = frags.next() {
                out.push_str(&collapse_whitespace(frag));
            }
            rest = &rest[idx + "<frag-marker>".len()..];
        }
        out.push_str(rest);
        attributish_prefix = out;
    }

    // Re-parse the attributish prefix as `row_syntax_table_args`.
    let (attrs, _sep) =
        crate::wikitext::tokenizer_v2::tokenize_table_cell_attributes(&attributish_prefix);
    if attrs.is_empty() {
        return;
    }

    // Sanitize and transfer attributes onto the cell (`applySanitizedArgs`).
    let tag = name(cell);
    let sanitized =
        crate::sanitizer::sanitize_tag_attrs(&tag, attrs, |proto| config.has_valid_protocol(proto));
    for kv in &sanitized {
        cell.set_attr(kv.key.to_string(), kv.value.to_string());
    }

    // Reparsed cells are non-mergeable and preserve that property.
    if let Some(dp) = cell.dp.as_mut() {
        dp.tmp.merged_table_cell = true;
        dp.tmp.table_cell_with_no_attribute_syntax = false;
    }

    // If the transclusion was embedded within the cell, lift the about group
    // up to the cell itself.
    let transclusions = content.transclusions;
    if !transclusions.is_empty() {
        let is_cell_itself = transclusions
            .first()
            .and_then(|t| t.get_attr("about"))
            .is_some_and(|a| cell.get_attr("about") == Some(a));
        if !is_cell_itself || transclusions.len() > 1 {
            hoist_transclusion_info(cell, &transclusions, source);
        }
    }

    // Drop the content consumed by the reparsed attribute prefix:
    // `preg_replace('/^[^|]*\|/', '', innerHTML)`.
    drop_consumed_prefix(cell);
}

/// `hoistTransclusionInfo` — lift the transclusion's `about`/`typeof`/`data-mw`
/// onto the cell, and strip the inner transclusion's encapsulation (unwrapping
/// wrapper spans, mirroring the cell-wrapping loop at the end of the PHP fn).
fn hoist_transclusion_info(cell: &mut Node, transclusions: &[Node], source: Option<&str>) {
    // Initialize the cell's DSR from the first transclusion when absent.
    if !is_valid_dsr(cell.dp.as_ref().and_then(|d| d.dsr.as_ref()))
        && let Some(tpl_dsr) = transclusions
            .first()
            .and_then(|t| t.dp.as_ref())
            .and_then(|d| d.dsr.clone())
        && is_valid_dsr(Some(&tpl_dsr))
        && let Some(cell_dp) = cell.dp.as_mut()
    {
        cell_dp.dsr = Some(tpl_dsr);
    }

    let cell_dsr = cell.dp.as_ref().and_then(|d| d.dsr.clone());

    let mut parts: Vec<serde_json::Value> = Vec::new();
    let mut pi: Vec<serde_json::Value> = Vec::new();
    let mut index: usize = 0;
    let mut about_ids: Vec<Option<String>> = Vec::new();

    let mut prev_dsr: Option<DomSourceRange> = None;
    let mut last_about: Option<String> = None;

    for transclusion in transclusions {
        let tpl_about = transclusion.get_attr("about").map(str::to_string);
        about_ids.push(tpl_about.clone());
        if transclusion.get_attr("about").is_some() {
            last_about = tpl_about.clone();
        }

        let tpl_dsr = transclusion.dp.as_ref().and_then(|d| d.dsr.clone());

        // Plug DSR gaps between transclusions.
        if let (Some(cell_dsr), Some(tpl_dsr)) = (cell_dsr.as_ref(), tpl_dsr.as_ref()) {
            if let Some(prev) = prev_dsr.as_ref() {
                if let (Some(a), Some(b)) = (prev.end, tpl_dsr.start) {
                    fill_dsr_gap(&mut parts, source, a, b);
                }
            } else if let (Some(a), Some(b)) = (cell_dsr.start, tpl_dsr.start) {
                fill_dsr_gap(&mut parts, source, a, b);
            }
        }

        // Assimilate the transclusion's data-mw parts.
        if let Some(dmw_json) = transclusion.data_mw.as_deref()
            && let Ok(dmw) = serde_json::from_str::<serde_json::Value>(dmw_json)
            && let Some(dmw_parts) = dmw.get("parts").and_then(|p| p.as_array())
        {
            for part in dmw_parts {
                if part.is_string() {
                    parts.push(part.clone());
                } else {
                    let mut obj = part.clone();
                    // Template index is relative to the other transclusions.
                    if let Some(obj_map) = obj.as_object_mut()
                        && let Some(tpl_obj) = obj_map.values_mut().find_map(|v| v.as_object_mut())
                    {
                        tpl_obj.insert("i".to_string(), serde_json::Value::from(index));
                    }
                    index += 1;
                    parts.push(obj);
                }
            }
        }

        // Accumulate `pi` from the transclusion's data-parsoid.
        if let Some(pi_json) = transclusion.dp.as_ref().and_then(|d| d.pi.clone())
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&pi_json)
        {
            match v {
                serde_json::Value::Array(arr) => pi.extend(arr),
                other => pi.push(other),
            }
        } else {
            pi.push(serde_json::Value::Array(vec![]));
        }

        prev_dsr = tpl_dsr;
    }

    // Trailing gap after the last transclusion.
    if let (Some(cell_dsr), Some(prev)) = (cell_dsr.as_ref(), prev_dsr.as_ref())
        && let (Some(a), Some(b)) = (prev.end, cell_dsr.end)
    {
        fill_dsr_gap(&mut parts, source, a, b);
    }

    // Hoist the transclusion info onto the cell.
    if let Some(about) = last_about {
        cell.set_attr("about", about);
    }
    add_type_of(cell, "mw:Transclusion");
    cell.data_mw = Some(serde_json::json!({ "parts": parts }).to_string());
    if !pi.is_empty()
        && let Some(dp) = cell.dp.as_mut()
    {
        dp.pi = Some(serde_json::Value::Array(pi).to_string());
    }

    // Remove the encapsulation from the (now absorbed) inner transclusion
    // children: strip `about` + `mw:Transclusion` and unwrap useless spans.
    strip_inner_encapsulation(cell, &about_ids);
}

fn is_valid_dsr(dsr: Option<&crate::wikitext::tokens_v2::DomSourceRange>) -> bool {
    dsr.is_some_and(|d| d.start.is_some() && d.end.is_some())
}

fn fill_dsr_gap(
    parts: &mut Vec<serde_json::Value>,
    source: Option<&str>,
    offset1: usize,
    offset2: usize,
) {
    if offset1 < offset2
        && let Some(src) = source
        && let Some(gap) = src.get(offset1..offset2)
    {
        parts.push(serde_json::Value::String(gap.to_string()));
    }
}

/// Add `ty` to the node's space-separated `typeof` attribute (if absent).
fn add_type_of(node: &mut Node, ty: &str) {
    let cur = node.get_attr("typeof").map(str::to_string);
    let merged = match cur {
        Some(c) if c.split_whitespace().any(|t| t == ty) => c,
        Some(c) => format!("{c} {ty}"),
        None => ty.to_string(),
    };
    node.set_attr("typeof", merged);
}

/// Remove `ty` from the node's `typeof` (if present).
fn remove_type_of(node: &mut Node, ty: &str) {
    if let Some(c) = node.get_attr("typeof").map(str::to_string) {
        let kept: Vec<&str> = c.split_whitespace().filter(|t| *t != ty).collect();
        if kept.is_empty() {
            node.attrs.retain(|a| a.key != "typeof");
        } else {
            node.set_attr("typeof", kept.join(" "));
        }
    }
}

/// Walk the cell and strip encapsulation attributes from descendants whose
/// `about` is one of the absorbed about-ids; unwrap useless span wrappers.
fn strip_inner_encapsulation(cell: &mut Node, about_ids: &[Option<String>]) {
    let mut i = 0usize;
    while i < cell.children.len() {
        let child_about = cell.children[i].get_attr("about").map(str::to_string);
        let matches = about_ids
            .iter()
            .any(|id| id.as_deref() == child_about.as_deref() && id.is_some());

        if !matches {
            i += 1;
            continue;
        }

        // A `span` wrapper (marked via `dp.tmp.wrapper` or the serialized
        // `data-parsoid` `tmp.wrapper` flag) is unwrapped: its children migrate
        // into the cell and the span is removed.
        let is_wrapper_span = name(&cell.children[i]) == "span"
            && (cell.children[i].dp.as_ref().is_some_and(|d| d.tmp.wrapper)
                || cell.children[i]
                    .data_parsoid
                    .as_deref()
                    .is_some_and(|d| d.contains("\"wrapper\":true")));

        if is_wrapper_span {
            let mut span = cell.children.remove(i);
            let span_children = std::mem::take(&mut span.children);
            for child in span_children.into_iter().rev() {
                cell.children.insert(i, child);
            }
            // Re-examine the migrated children at the same index (they may also
            // carry `about` matches), then continue past them.
            continue;
        }

        // Otherwise, just strip the encapsulation attributes from the child.
        cell.children[i].attrs.retain(|a| a.key != "about");
        remove_type_of(&mut cell.children[i], "mw:Transclusion");
        i += 1;
    }
}

/// `DOMCompat::setInnerHTML(cell, preg_replace('/^[^|]*\|/','', …))` — drop the
/// leading text (and any nodes) consumed by the reparsed attribute prefix.
fn drop_consumed_prefix(cell: &mut Node) {
    let mut consumed_pipe = false;
    let mut i = 0usize;
    while i < cell.children.len() && !consumed_pipe {
        match &cell.children[i].kind {
            NodeKind::Text(t) => {
                if let Some(pipe) = t.find('|') {
                    let rest = t[pipe + 1..].to_string();
                    consumed_pipe = true;
                    if rest.is_empty() {
                        cell.children.remove(i);
                    } else {
                        cell.children[i].kind = NodeKind::Text(rest);
                        i += 1;
                    }
                } else {
                    cell.children.remove(i);
                }
            }
            NodeKind::Element(_) if has_type_of(&cell.children[i], "mw:Entity") => {
                cell.children.remove(i);
            }
            NodeKind::Comment(_) => {
                cell.children.remove(i);
            }
            _ => break,
        }
    }
}

/// `fromWellBalancedTemplate` — data-mw has exactly one `parts` entry.
fn from_well_balanced_template(node: &Node) -> bool {
    node.data_mw
        .as_deref()
        .and_then(|dm| serde_json::from_str::<serde_json::Value>(dm).ok())
        .and_then(|v| {
            v.get("parts")
                .and_then(|p| p.as_array())
                .map(|p| p.len() == 1)
        })
        .unwrap_or(false)
}

fn cell_tmp_flag(cell: &Node, f: impl Fn(&crate::wikitext::tokens_v2::TempData) -> bool) -> bool {
    cell.dp.as_ref().map(|d| f(&d.tmp)).unwrap_or(false)
}

/// `getReparseType` — decide whether to merge/reparse/split.
///
/// The merge-with-previous-cell path is not yet wired (it requires DSR recovery
/// of preceding cells and `convertAttribsToContent`); we only handle the
/// reparse/split decisions here.
fn get_reparse_type(cell: &Node) -> ReparseScenario {
    let cell_is_td = name(cell) == "td";
    let test_re = if cell_is_td { "|" } else { "!|" };
    let no_attr_reparsing = !cell_tmp_flag(cell, |t| t.table_cell_with_no_attribute_syntax)
        || (cell_tmp_flag(cell, |t| t.non_mergeable_table_cell)
            && cell.dp.as_ref().and_then(|d| d.stx.as_deref()) != Some("row"));
    pipe_status_in_content(cell, test_re, false, no_attr_reparsing)
}

/// `pipeStatusInContent` — search the cell content for a `|` (or `!` for `<th>`)
/// in templated content, deciding reparse vs. split.
fn pipe_status_in_content(
    node: &Node,
    test_re: &str,
    in_tpl_content: bool,
    no_attr_reparsing: bool,
) -> ReparseScenario {
    let mut about: Option<String> = None;
    for child in &node.children {
        if in_tpl_content && matches!(child.kind, NodeKind::Text(_)) && text_matches(child, test_re)
        {
            return if no_attr_reparsing {
                ReparseScenario::MaybeSplitCell
            } else {
                ReparseScenario::MaybeReparseAttrs
            };
        }

        if matches!(child.kind, NodeKind::Element(_)) {
            let child_about = child.get_attr("about").map(str::to_string);
            if about.is_some() && child_about != about {
                about = None;
            }
            if about.is_none() && has_type_of(child, "mw:Transclusion") {
                about = child_about;
            }

            if !has_type_of(child, "mw:DOMFragment") {
                let status =
                    pipe_status_in_content(child, test_re, about.is_some(), no_attr_reparsing);
                if status != ReparseScenario::NotNeeded {
                    return status;
                }
            }
        }
    }
    ReparseScenario::NotNeeded
}

fn text_matches(node: &Node, re: &str) -> bool {
    match &node.kind {
        NodeKind::Text(t) if re == "|" => t.contains('|'),
        NodeKind::Text(t) => t.contains('|') || t.contains('!'),
        _ => false,
    }
}

/// `putsNextSiblingInSOLState` — the cell's inner HTML ends in a newline
/// (possibly followed by comments and whitespace). Retained for the merge
/// path; the current reparse/split path does not need it.
#[allow(dead_code)]
fn puts_next_sibling_in_sol_state(cell: &Node) -> bool {
    let text = text_content(cell);
    let trimmed = text.trim_end_matches([' ', '\t']);
    trimmed.ends_with('\n')
}

/// `run` — the `TableFixups` pass entry point. Traverses `table`/`td`/`th` nodes
/// and applies the reparse/split fixups (merge-with-previous deferred).
pub fn run(root: &mut Node, config: &dyn SiteConfig, source: Option<&str>) {
    let children = std::mem::take(&mut root.children);
    root.children = process_children(children, config, source);
}

fn process_children(
    mut children: Vec<Node>,
    config: &dyn SiteConfig,
    source: Option<&str>,
) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    for mut child in children.drain(..) {
        match &child.kind {
            NodeKind::Element(ElementKind::Table) => {
                // A well-balanced templated table is skipped wholesale.
                if has_type_of(&child, "mw:Transclusion") && from_well_balanced_template(&child) {
                    out.push(child);
                    continue;
                }
                let children = std::mem::take(&mut child.children);
                child.children = process_children(children, config, source);
                out.push(child);
            }
            NodeKind::Element(ElementKind::TableCell | ElementKind::TableHeader) => {
                let is_templated = has_type_of(&child, "mw:Transclusion");
                let cell_name = name(&child);
                let reparse_type = get_reparse_type(&child);

                // Special `<th>` "!! foo" case.
                if cell_name == "th"
                    && is_templated
                    && reparse_type != ReparseScenario::MaybeCombineWithPrevCell
                    && cell_tmp_flag(&child, |t| t.table_cell_with_no_attribute_syntax)
                    && let Some(first) = child.children.iter_mut().find(|c| {
                        matches!(c.kind, NodeKind::Text(_))
                            || matches!(c.kind, NodeKind::Element(_))
                    })
                    && let NodeKind::Text(t) = &mut first.kind
                    && let Some(rest) = t.strip_prefix('!')
                {
                    *t = rest.to_string();
                    if let Some(dp) = child.dp.as_mut() {
                        dp.stx = Some("row".to_string());
                        dp.tmp.non_mergeable_table_cell = true;
                    }
                }

                if reparse_type == ReparseScenario::MaybeReparseAttrs
                    && cell_tmp_flag(&child, |t| t.table_cell_with_no_attribute_syntax)
                {
                    let template_wrapper = if is_templated {
                        Some(child.clone())
                    } else {
                        None
                    };
                    reparse_templated_attributes(&mut child, template_wrapper, config, source);
                }

                // Split hidden cells on `||`/`!!`, recursing into the remainder.
                let split = split_hidden_cells(&mut child, &cell_name, source);
                let children = std::mem::take(&mut child.children);
                child.children = process_children(children, config, source);
                out.push(child);
                out.extend(split);
            }
            NodeKind::Element(_) | NodeKind::Document => {
                let children = std::mem::take(&mut child.children);
                child.children = process_children(children, config, source);
                out.push(child);
            }
            NodeKind::Text(_) | NodeKind::Comment(_) => {
                out.push(child);
            }
        }
    }
    out
}

/// `split_hidden_cells` — split a `<td>`/`<th>` on an embedded `||`/`!!`,
/// returning the newly created sibling cells (inserted after the cell).
fn split_hidden_cells(cell: &mut Node, cell_name: &str, source: Option<&str>) -> Vec<Node> {
    let is_td = cell_name == "td";
    let orig_about = cell.get_attr("about").map(str::to_string);

    let mut new_cells: Vec<Node> = Vec::new();
    let mut needs_tpl_hoist = false;

    let mut i = 0usize;
    while i < cell.children.len() {
        let child_is_text = matches!(cell.children[i].kind, NodeKind::Text(_));
        let child_is_simple_span = is_simple_templated_span(&cell.children[i]);

        if !child_is_text && !child_is_simple_span {
            i += 1;
            continue;
        }

        let child_text = match &cell.children[i].kind {
            NodeKind::Text(t) => t.clone(),
            _ => text_content(&cell.children[i]),
        };

        let found = if is_td {
            find_first_sep(&child_text, "||")
        } else {
            find_first_sep_th(&child_text)
        };

        let Some((prefix, suffix)) = found else {
            i += 1;
            continue;
        };

        // Adjust the child's content to the prefix.
        let has_span_wrapper = child_is_simple_span;
        let span_about = if has_span_wrapper {
            cell.children[i].get_attr("about").map(str::to_string)
        } else {
            None
        };
        match &mut cell.children[i].kind {
            NodeKind::Text(t) => *t = prefix.clone(),
            _ => {
                if has_span_wrapper {
                    // Replace the span with the prefix text (its encapsulation is
                    // hoisted below).
                    cell.children[i] = Node::text(prefix.clone());
                }
            }
        }

        // Build the new cell.
        let mut new_cell = Node::element(if is_td {
            ElementKind::TableCell
        } else {
            ElementKind::TableHeader
        });
        if !suffix.is_empty() {
            new_cell.push_child(Node::text(suffix));
        }
        let new_dp = DataParsoid {
            stx: Some("row".to_string()),
            tmp: crate::wikitext::tokens_v2::TempData {
                table_cell_with_no_attribute_syntax: true,
                non_mergeable_table_cell: true,
                ..Default::default()
            },
            ..Default::default()
        };
        new_cell.dp = Some(new_dp);

        let about = if has_span_wrapper {
            span_about
        } else {
            orig_about.clone()
        };
        if let Some(a) = &about {
            new_cell.set_attr("about", a.clone());
            needs_tpl_hoist = true;
        }

        new_cells.push(new_cell);
        // Continue scanning from the same index; children after the split were
        // already relocated into the new cell.
    }

    if needs_tpl_hoist {
        // The `about` is now on multiple cells; hoist the transclusion info to
        // the original cell (which keeps the data-mw). This mirrors the
        // `hoistTransclusionInfo` call + `tplInfo->last` update.
        let transclusions = collect_transclusions(cell);
        if !transclusions.is_empty() {
            hoist_transclusion_info(cell, &transclusions, source);
        }
    }

    new_cells
}

fn collect_transclusions(node: &Node) -> Vec<Node> {
    let mut out = Vec::new();
    for child in &node.children {
        if has_type_of(child, "mw:Transclusion") {
            out.push(child.clone());
        }
        if matches!(child.kind, NodeKind::Element(_)) {
            out.extend(collect_transclusions(child));
        }
    }
    out
}

/// Find the `sep` separator: return `(prefix, suffix)`, or `None`.
fn find_first_sep(text: &str, sep: &str) -> Option<(String, String)> {
    let idx = text.find(sep)?;
    Some((text[..idx].to_string(), text[idx + sep.len()..].to_string()))
}

/// For `<th>`, find the shortest of `||` or `!!`.
fn find_first_sep_th(text: &str) -> Option<(String, String)> {
    let d = text.find("||").map(|i| (i, 2));
    let e = text.find("!!").map(|i| (i, 2));
    match (d, e) {
        (Some((di, _)), Some((ei, _))) => {
            if di <= ei {
                find_first_sep(text, "||")
            } else {
                find_first_sep(text, "!!")
            }
        }
        (Some(_), None) => find_first_sep(text, "||"),
        (None, Some(_)) => find_first_sep(text, "!!"),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockSiteConfig;

    fn templated_cell(text: &str) -> Node {
        // Build the DOM shape produced for `|{{tpl}}` where the template expands
        // to `text`: a `<td>` with no attribute box, whose content is wrapped in
        // a `mw:Transclusion` encapsulation span (the WRAPPER target).
        let mut td = Node::element(ElementKind::TableCell);
        td.dp = Some(DataParsoid {
            tmp: crate::wikitext::tokens_v2::TempData {
                table_cell_with_no_attribute_syntax: true,
                ..Default::default()
            },
            ..Default::default()
        });
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:Transclusion");
        span.set_attr("about", "#mwt1");
        span.push_child(Node::text(text));
        span.data_mw =
            Some("{\"parts\":[{\"template\":{\"target\":{\"wt\":\"tpl\"}}}]}".to_string());
        span.dp = Some(DataParsoid {
            tmp: crate::wikitext::tokens_v2::TempData {
                wrapper: true,
                ..Default::default()
            },
            ..Default::default()
        });
        td.push_child(span);
        td
    }

    #[test]
    fn test_attributish_prefix() {
        assert_eq!(
            attributish_prefix("style=\"color:red;\"|Foo").as_deref(),
            Some("style=\"color:red;\"|")
        );
        // A `||` after the pipe is not a usable separator.
        assert_eq!(attributish_prefix("a||b"), None);
        // No pipe at all.
        assert_eq!(attributish_prefix("Foo"), None);
    }

    #[test]
    fn test_pipe_in_text() {
        assert!(pipe_in_text("a|b"));
        assert!(pipe_in_text("|b"));
        assert!(pipe_in_text("a|"));
        assert!(!pipe_in_text("a||b"));
        assert!(!pipe_in_text("plain"));
    }

    #[test]
    fn test_reparse_templated_attributes_applies_kv() {
        let config = MockSiteConfig::new();
        // `|{{table_attribs}}` where the template expands to `style="color:red;"|Foo`.
        let mut cell = templated_cell("style=\"color:red;\"|Foo");
        reparse_templated_attributes(&mut cell, None, &config, None);

        // The attribute is hoisted onto the cell and the consumed content dropped.
        assert_eq!(cell.get_attr("style"), Some("color:red;"));
        // The transclusion was hoisted onto the cell (about/typeof).
        assert_eq!(cell.get_attr("about"), Some("#mwt1"));
        assert!(has_type_of(&cell, "mw:Transclusion"));
        // The wrapper span was unwrapped; only the `Foo` content remains.
        let text = text_content(&cell);
        assert_eq!(text, "Foo");
    }
}
