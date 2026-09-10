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

        // A transclusion marker meta (`mw:Transclusion` / `…/End`) that has been
        // absorbed into the cell is removed entirely (its data-mw was already
        // hoisted).
        let is_marker_meta = matches!(&cell.children[i].kind, NodeKind::Element(ElementKind::Other(name)) if name == "meta")
            && cell.children[i]
                .get_attr("typeof")
                .is_some_and(|t| t == "mw:Transclusion" || t.ends_with("/End"));
        if is_marker_meta {
            cell.children.remove(i);
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

/// `WTUtils::hasLiteralHTMLMarker` — the cell's `stx` is `"html"`.
fn has_literal_html_marker(cell: &Node) -> bool {
    cell.dp
        .as_ref()
        .is_some_and(|d| d.stx.as_deref() == Some("html"))
}

/// `Utils::isValidDSR($dsr, true)` — start, end, openWidth and closeWidth are
/// all non-null.
fn valid_dsr_with_ws(dsr: Option<&DomSourceRange>) -> bool {
    dsr.is_some_and(|d| {
        d.start.is_some() && d.end.is_some() && d.open_width.is_some() && d.close_width.is_some()
    })
}

/// Extract the cell's source wikitext via `$dsr->substr($source)` (PHP
/// `DomSourceRange::substr`). Returns `None` when the DSR is invalid or the
/// source is unavailable.
fn dsr_substr(dsr: &DomSourceRange, source: Option<&str>) -> Option<String> {
    let (Some(start), Some(end)) = (dsr.start, dsr.end) else {
        return None;
    };
    source.and_then(|s| s.get(start..end).map(str::to_string))
}

/// `DomSourceRange::innerSubstr` — the cell's content, between the open/close
/// tag widths.
fn dsr_inner_substr(dsr: &DomSourceRange, source: Option<&str>) -> Option<String> {
    let (Some(start), Some(end)) = (dsr.start, dsr.end) else {
        return None;
    };
    let inner_start = start + dsr.open_width.unwrap_or(0);
    let inner_end = end.saturating_sub(dsr.close_width.unwrap_or(0));
    source.and_then(|s| s.get(inner_start..inner_end).map(str::to_string))
}

/// `stripTrailingPipe` — remove the trailing `|` (td) / `!` (th) from the last
/// text descendant of `cell`, returning the stripped char (or `None`).
fn strip_trailing_pipe(cell: &mut Node) -> Option<String> {
    // Find the last text descendant.
    fn last_text_mut(node: &mut Node) -> Option<&mut Node> {
        let mut cur = node;
        loop {
            if matches!(cur.kind, NodeKind::Text(_)) {
                return Some(cur);
            }
            let last_child_idx = cur.children.len().checked_sub(1)?;
            cur = &mut cur.children[last_child_idx];
        }
    }
    let last = last_text_mut(cell)?;
    let NodeKind::Text(t) = &mut last.kind else {
        return None;
    };
    let stripped = t.pop()?.to_string();
    Some(stripped)
}

const PARSOID_ATTRIBUTES: [&str; 5] = [
    "data-object-id",
    "typeof",
    "about",
    "data-parsoid",
    "data-mw",
];

/// `transferSourceBetweenCells` — move a leading source substring (a pipe/`!`)
/// from `from` to `to`, adjusting DSRs and data-mw (mirrors PHP).
fn transfer_source_between_cells(
    src: &str,
    from: &mut Node,
    to: &mut Node,
    empty_from_content: bool,
) {
    // If `to` is a transclusion wrapper, prepend `src` to its data-mw parts.
    if has_type_of(to, "mw:Transclusion")
        && let Some(dmw_json) = to.data_mw.as_deref()
        && let Ok(mut dmw) = serde_json::from_str::<serde_json::Value>(dmw_json)
    {
        if let Some(parts) = dmw.get_mut("parts").and_then(|p| p.as_array_mut()) {
            parts.insert(0, serde_json::Value::String(src.to_string()));
        }
        to.data_mw = Some(dmw.to_string());
    }

    let row_syntax_char = if name(to) == "td" { "|" } else { "!" };
    if let Some(from_dp) = from.dp.as_mut()
        && row_syntax_char == "|"
    {
        from_dp.start_tag_src = None;
        from_dp.attr_sep_src = None;
    }

    let has_row_syntax = src.ends_with(row_syntax_char);
    if has_row_syntax && let Some(to_dp) = to.dp.as_mut() {
        to_dp.stx = Some("row".to_string());
    }

    let src_len = src.chars().count();
    if let Some(to_dp) = to.dp.as_mut()
        && let Some(to_dsr) = to_dp.dsr.as_mut()
    {
        if let Some(start) = to_dsr.start.as_mut() {
            *start = start.saturating_sub(src_len);
        }
        if has_row_syntax && let Some(ow) = to_dsr.open_width.as_mut() {
            *ow += 1;
        }
    }
    if let Some(from_dp) = from.dp.as_mut()
        && let Some(from_dsr) = from_dp.dsr.as_mut()
    {
        if let Some(end) = from_dsr.end.as_mut() {
            *end = end.saturating_sub(src_len);
        }
        if has_row_syntax
            && empty_from_content
            && let Some(ow) = from_dsr.open_width.as_mut()
        {
            *ow = ow.saturating_sub(1);
        }
    }
}

/// `mergeCells` — merge `from` into `to`: copy attributes (except the parsoid
/// set for identical cell types), migrate `from`'s children, and drop `from`.
/// Mirrors PHP's `mergeCells($fromSrc, $from, $to)`, which always moves `from`'s
/// attributes+children onto `to` (for non-identical types, `from` is the
/// authoritative cell and its transclusion attributes are copied onto `to` too).
fn merge_cells(from_src: &str, from: &mut Node, to: &mut Node) {
    transfer_source_between_cells(from_src, from, to, false);

    let identical_cell_types = name(from) == name(to);
    let ignore_parsoid = identical_cell_types;

    // For identical cell types, `from`'s plain attributes migrate to `to`
    // (skipping the parsoid metadata set). For non-identical types (td/th),
    // `from` is authoritative, so also copy its parsoid metadata (about/typeof/
    // data-mw) across — those live on `from` and must move to `to`.
    for attr in from.attrs.clone() {
        if !ignore_parsoid || !PARSOID_ATTRIBUTES.contains(&attr.key.as_str()) {
            to.set_attr(attr.key.clone(), attr.value.clone());
        }
    }
    // Transclusion metadata that is not in the exported `attrs` list (data-mw
    // is a dedicated field) transfers when `from` holds it.
    if !identical_cell_types || to.data_mw.is_none() {
        if let Some(dmw) = from.data_mw.clone() {
            to.data_mw = Some(dmw);
        }
        if from.dp.as_ref().and_then(|d| d.pi.clone()).is_some()
            && let Some(dp) = to.dp.as_mut()
        {
            dp.pi = from.dp.as_ref().and_then(|d| d.pi.clone());
        }
    }

    // Migrate `from`'s children into `to` (at the front for identical types,
    // since `from` precedes `to` in source order).
    let from_children = std::mem::take(&mut from.children);
    let insert_at = if identical_cell_types {
        0
    } else {
        to.children.len()
    };
    for child in from_children.into_iter().rev() {
        to.children.insert(insert_at, child);
    }

    // The merged cell can't merge further.
    if let Some(dp) = to.dp.as_mut() {
        dp.tmp.merged_table_cell = true;
        dp.tmp.table_cell_with_no_attribute_syntax = false;
    }
}

/// `convertAttribsToContent` — reinterpret a cell's attribute wikitext as cell
/// content, moving it into the cell as leading text and dropping the literal
/// attributes. Mirrors the `!preg_match("#['[{<]#")` optimized (plain string)
/// branch; the reparse-through-nested-pipeline branch for richer source is not
/// wired (rare in our fixtures).
fn convert_attribs_to_content(cell: &mut Node, leading_pipe: bool, trailing_pipe: bool) {
    let cell_attr_src = cell
        .dp
        .as_ref()
        .and_then(|d| d.tmp.attr_src.clone())
        .unwrap_or_default();

    if has_type_of(cell, "mw:ExpandedAttrs") {
        remove_type_of(cell, "mw:ExpandedAttrs");
        if let Some(dmw_json) = cell.data_mw.as_deref()
            && let Ok(mut dmw) = serde_json::from_str::<serde_json::Value>(dmw_json)
        {
            if let Some(obj) = dmw.as_object_mut() {
                obj.remove("attribs");
            }
            cell.data_mw = Some(dmw.to_string());
        }
    }

    let leading_pipe_char = if name(cell) == "td" { "|" } else { "!" };
    // Plain-string optimization (no `'[{<` constructs).
    let mut text = String::new();
    if leading_pipe {
        text.push_str(leading_pipe_char);
    }
    text.push_str(&cell_attr_src);
    if !cell_attr_src.is_empty() && trailing_pipe {
        text.push('|');
    }
    if !text.is_empty() {
        cell.children.insert(0, Node::text(text));
    }

    // Remove the (now-content) literal attributes, keeping parsoid attrs.
    cell.attrs
        .retain(|a| PARSOID_ATTRIBUTES.contains(&a.key.as_str()));

    // Drop shadow attributes to suppress them from wt2wt output.
    if let Some(dp) = cell.dp.as_mut() {
        dp.a = None;
        dp.sa = None;
        dp.tmp.table_cell_with_no_attribute_syntax = true;
    }
}

/// `reparseWithPreviousCell` — the merge-cell driver. Given the previous cell
/// and the current cell, examine their combined source syntax and either merge
/// or transfer a leading pipe/`!` between them. Returns the number of original
/// cells (in document order) consumed by the operation: `1` when the current
/// cell remains (no full merge), or `2` when the current cell was merged into
/// the previous one.
fn reparse_with_previous_cell(
    prev: &mut Node,
    cell: &mut Node,
    config: &dyn SiteConfig,
    source: Option<&str>,
) -> usize {
    let prev_is_td = name(prev) == "td";
    let prev_has_attrs = !cell_tmp_flag(prev, |t| t.table_cell_with_no_attribute_syntax);

    let cell_is_td = name(cell) == "td";
    let cell_has_attrs = !cell_tmp_flag(cell, |t| t.table_cell_with_no_attribute_syntax);

    // Recover the previous cell's source, using `tsr` start (DSR may have been
    // expanded to include fostered content).
    let prev_dsr = prev.dp.as_ref().and_then(|d| d.dsr.clone());
    let prev_tsr_start = prev
        .dp
        .as_ref()
        .and_then(|d| d.tsr.as_ref())
        .and_then(|t| t.start);
    let prev_cell_src = match (&prev_dsr, prev_tsr_start) {
        (Some(dsr), Some(_tsr)) => dsr_substr(dsr, source),
        _ => None,
    };
    let prev_cell_content = match (&prev_dsr, prev_tsr_start) {
        (Some(dsr), Some(tsr)) => {
            // Use tsr->start (mirrors PHP `$prevDsr->start = $prevDp->tsr->start`).
            let adjusted = DomSourceRange {
                start: Some(tsr),
                ..dsr.clone()
            };
            dsr_inner_substr(&adjusted, source)
        }
        _ => None,
    };

    let prev_has_trailing_pipe = match &prev_cell_content {
        Some(c) => {
            (cell_is_td && c.ends_with('|')) || (!cell_is_td && !prev_is_td && c.ends_with('!'))
        }
        None => false,
    };

    if prev_has_trailing_pipe {
        // `$prev` is `..|` → no merge; strip the `|` and migrate it to `$cell`.
        let Some(stripped) = strip_trailing_pipe(prev) else {
            return 1;
        };
        transfer_source_between_cells(&stripped, prev, cell, false);
        return 1;
    }

    if prev_is_td
        && cell_tmp_flag(prev, |t| t.non_mergeable_table_cell)
        && prev.dp.as_ref().and_then(|d| d.stx.as_deref()) != Some("row")
    {
        if prev_cell_content.as_deref().is_some_and(|c| !c.is_empty()) {
            // `$prev` is `||..` in SOL position with content.
            convert_attribs_to_content(cell, true, true);
            merge_cells(prev_cell_src.as_deref().unwrap_or(""), prev, cell);
            2
        } else {
            // `$prev` is `||` in SOL position, no content → just migrate `|`.
            transfer_source_between_cells("|", prev, cell, true);
            1
        }
    } else if !prev_has_attrs {
        // `$prev` has no attributes → merge `$prev` into `$cell`.
        if cell_is_td && cell_has_attrs {
            convert_attribs_to_content(cell, false, true);
        }

        if !cell_is_td && !cell_has_attrs {
            // `<th>` without attributes: its `!` becomes content.
            cell.children.insert(0, Node::text("!"));
        } else if prev_cell_content.as_deref().is_some_and(|c| !c.is_empty()) {
            // `$prev`'s content becomes `$cell`'s attributes.
            let reparse_src = prev_cell_content.as_deref().unwrap_or("").to_string() + "|";
            let (attrs, _sep) =
                crate::wikitext::tokenizer_v2::tokenize_table_cell_attributes(&reparse_src);
            if !attrs.is_empty() {
                let tag = name(cell);
                let sanitized = crate::sanitizer::sanitize_tag_attrs(&tag, attrs, |proto| {
                    config.has_valid_protocol(proto)
                });
                for kv in &sanitized {
                    cell.set_attr(kv.key.to_string(), kv.value.to_string());
                }
                prev.children.clear();
            } else {
                if cell_is_td {
                    cell.children.insert(0, Node::text("|"));
                } else if cell_has_attrs {
                    convert_attribs_to_content(cell, true, true);
                }
            }
        }

        merge_cells(prev_cell_src.as_deref().unwrap_or(""), prev, cell);
        2
    } else if prev_cell_content.as_deref().is_none_or(|c| c.is_empty()) {
        // `$prev` has attributes and empty content → attrs become content.
        convert_attribs_to_content(prev, false, false);
        transfer_source_between_cells("|", prev, cell, true);
        1
    } else {
        // `$prev` has attributes and content → `$cell` merges into `$prev`.
        // (Faithful to PHP: convert `$cell`'s attrs to content, then
        // `mergeCells($prevCellSrc, $prev, $cell)` absorbs `$prev` into `$cell`.)
        convert_attribs_to_content(cell, true, true);
        merge_cells(prev_cell_src.as_deref().unwrap_or(""), prev, cell);
        2
    }
}

/// `getReparseType` — decide whether to merge/reparse/split.
///
/// The merge decision (PHP Conditions 1–6) precedes the `pipeStatusInContent`
/// reparse/split check; it needs the previous *element* sibling and valid DSR on
/// both cells.
fn get_reparse_type(cell: &Node, in_tpl_content: bool, prev: Option<&Node>) -> ReparseScenario {
    let cell_is_td = name(cell) == "td";
    let cell_dp = cell.dp.as_ref();

    // The merge-with-previous-cell decision (`maybeCombineWithPrevCell`).
    if let (Some(prev), Some(prev_dp)) = (prev, prev.and_then(|p| p.dp.as_ref())) {
        let prev_is_element = matches!(prev.kind, NodeKind::Element(_));
        if prev_is_element
            // Condition 3: cell came from the start of a template source.
            && cell_tmp_flag(cell, |t| t.at_src_start)
            // Condition 4: not already merged / not failed.
            && !cell_tmp_flag(cell, |t| t.merged_table_cell)
            && !cell_tmp_flag(cell, |t| t.failed_reparse)
            // Conditions 1 & 2: prev is a non-HTML cell with valid DSR.
            && !has_literal_html_marker(prev)
            && valid_dsr_with_ws(prev_dp.dsr.as_ref())
            // Condition 5: not unmergeable (unless td→th).
            && (!cell_tmp_flag(cell, |t| t.non_mergeable_table_cell)
                || (name(prev) == "td" && !cell_is_td))
            // Condition 6: prev doesn't put cell in SOL state.
            && !puts_next_sibling_in_sol_state(prev)
        {
            return ReparseScenario::MaybeCombineWithPrevCell;
        }
    }

    let test_re = if cell_is_td { "|" } else { "!|" };
    let no_attr_reparsing = !cell_tmp_flag(cell, |t| t.table_cell_with_no_attribute_syntax)
        || (cell_tmp_flag(cell, |t| t.non_mergeable_table_cell)
            && cell_dp.and_then(|d| d.stx.as_deref()) != Some("row"));
    pipe_status_in_content(cell, test_re, in_tpl_content, no_attr_reparsing)
}

/// `pipeStatusInContent` — search the cell content for a `|` (or `!` for `<th>`)
/// in templated content, deciding reparse vs. split. Faithful port of PHP's
/// `TableFixups::pipeStatusInContent`, carrying `in_tpl_content`/`about` state
/// across siblings (a text node following a transclusion is still "in-template").
fn pipe_status_in_content(
    node: &Node,
    test_re: &str,
    in_tpl_content: bool,
    no_attr_reparsing: bool,
) -> ReparseScenario {
    let mut in_tpl_content = in_tpl_content;
    let mut no_attr_reparsing = no_attr_reparsing;
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
                in_tpl_content = false;
                about = None;
            }
            if !in_tpl_content && has_type_of(child, "mw:Transclusion") {
                in_tpl_content = true;
                about = child_about;
            }

            if !has_type_of(child, "mw:DOMFragment") {
                // `|`/`!` chars in extension/language-variant content don't
                // trigger cell parsing (higher tokenization precedence).
                if should_abort_attr(child) {
                    no_attr_reparsing = true;
                }
                let status =
                    pipe_status_in_content(child, test_re, in_tpl_content, no_attr_reparsing);
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
/// (possibly followed by comments and whitespace).
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
    // Transclusion marker metas (`mw:Transclusion` / `mw:Transclusion/End`) that
    // lost their data-mw to the `hoistTransclusionInfo` lift are stray: tplwrap
    // had already consumed the matched pair in the well-balanced case, so any
    // leftover marker without data-mw is an artifact of the cross-cell split and
    // is dropped (mirrors the `stripMetaTags` net effect).
    remove_stray_marker_metas(root);
}

/// Remove transclusion marker metas (`mw:Transclusion` / `mw:Transclusion/End`)
/// that no longer carry `data-mw` (their info was hoisted).
fn remove_stray_marker_metas(node: &mut Node) {
    node.children.retain(|c| {
        if matches!(&c.kind, NodeKind::Element(ElementKind::Other(name)) if name == "meta")
            && c.data_mw.is_none()
            && c.get_attr("typeof").is_some_and(|t| {
                t == "mw:Transclusion"
                    || t == "mw:Transclusion/End"
                    || t == "mw:Param"
                    || t == "mw:Param/End"
            })
        {
            return false;
        }
        true
    });
    for child in &mut node.children {
        remove_stray_marker_metas(child);
    }
}

fn process_children(
    children: Vec<Node>,
    config: &dyn SiteConfig,
    source: Option<&str>,
) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(children.len());
    let mut i = 0;
    while i < children.len() {
        let child = children[i].clone();
        // The immediate previous sibling. Use the last *emitted* node in `out`
        // (which reflects merges), mirroring PHP's live-DOM `$cell->previousSibling`:
        // when the previous cell was merged into the cell before it, the previous
        // sibling is the merged result, not the absorbed cell.
        let prev_sibling: Option<Node> = out.last().cloned().or_else(|| {
            if i > 0 {
                Some(children[i - 1].clone())
            } else {
                None
            }
        });
        match &child.kind {
            NodeKind::Element(ElementKind::Table) => {
                // A well-balanced templated table is skipped wholesale.
                if has_type_of(&child, "mw:Transclusion") && from_well_balanced_template(&child) {
                    out.push(child);
                    i += 1;
                    continue;
                }
                let mut child = child;
                let children = std::mem::take(&mut child.children);
                child.children = process_children(children, config, source);
                out.push(child);
            }
            NodeKind::Element(ElementKind::TableCell | ElementKind::TableHeader) => {
                // The merge-with-previous-cell path: when this cell should
                // combine with its *immediate* previous sibling, mutate both the
                // previous cell (already emitted to `out`) and this one in place.
                // Mirror PHP's `$cell->previousSibling`: the merge only fires when
                // the immediate previous sibling is an Element (a text/comment
                // separator between cells — e.g. across rows — prevents merging).
                if get_reparse_type(&child, false, prev_sibling.as_ref())
                    == ReparseScenario::MaybeCombineWithPrevCell
                {
                    // Find and mutate the previous cell in `out`.
                    if let Some(prev_out) = out.iter_mut().rev().find(|c| {
                        matches!(
                            c.kind,
                            NodeKind::Element(ElementKind::TableCell | ElementKind::TableHeader)
                        )
                    }) {
                        let mut cur = child.clone();
                        let consumed =
                            reparse_with_previous_cell(prev_out, &mut cur, config, source);
                        if consumed == 2 {
                            // `cur` is the merged result (prev was absorbed).
                            // Replace the previous cell in `out` with `cur`.
                            if let Some(pos) = out.iter().rposition(|c| {
                                matches!(
                                    c.kind,
                                    NodeKind::Element(
                                        ElementKind::TableCell | ElementKind::TableHeader
                                    )
                                )
                            }) {
                                out[pos] = cur;
                            }
                            i += 1;
                            continue;
                        }
                        // consumed == 1: `cur` remains a distinct cell; process it
                        // normally (reparse/split).
                        let mut processed =
                            process_cell(cur, config, source, false, prev_sibling.as_ref());
                        out.append(&mut processed);
                        i += 1;
                        continue;
                    }
                }

                let mut processed =
                    process_cell(child, config, source, false, prev_sibling.as_ref());
                out.append(&mut processed);
            }
            NodeKind::Element(_) | NodeKind::Document => {
                let mut child = child;
                let children = std::mem::take(&mut child.children);
                child.children = process_children(children, config, source);
                out.push(child);
            }
            NodeKind::Text(_) | NodeKind::Comment(_) => {
                out.push(child);
            }
        }
        i += 1;
    }
    out
}

/// Process a single `<td>`/`<th>` cell: apply the `<th>` `!! foo` special case,
/// reparse any templated `k=v|` attribute prefix, then split on hidden
/// `||`/`!!` separators. Return the resulting cells in document order — the
/// original cell first, then each newly split cell. `in_tpl` is true when this
/// cell was created by a split (so it inherits the enclosing transclusion
/// context, mirroring `dtState->tplInfo` in PHP).
fn process_cell(
    mut cell: Node,
    config: &dyn SiteConfig,
    source: Option<&str>,
    in_tpl: bool,
    prev_sibling: Option<&Node>,
) -> Vec<Node> {
    // A link/image inside this cell's attribute region terminated attribute
    // processing, so the attributes must be turned back into cell content
    // (mirrors PHP's `cellAttrTerminatorSeen` branch, which also clears the flag
    // and reprocesses the cell for further fixups).
    if cell_tmp_flag(&cell, |t| t.cell_attr_terminator_seen == Some(true)) {
        convert_attribs_to_content(&mut cell, false, true);
        if let Some(dp) = cell.dp.as_mut() {
            dp.tmp.cell_attr_terminator_seen = None;
        }
        // Reprocess in case this round makes the cell suitable for more fixups.
        return process_cell(cell, config, source, in_tpl, prev_sibling);
    }

    let is_templated = has_type_of(&cell, "mw:Transclusion");
    let cell_name = name(&cell);
    // The merge-with-previous decision is handled by the caller (`process_children`);
    // here `prev` is irrelevant, so pass `None` (no merge path reached).
    let reparse_type = get_reparse_type(&cell, in_tpl || is_templated, None);

    // Special `<th>` "!! foo" case: strip a leading `!` from a templated `<th>`
    // when its previous sibling is an element that does not put this cell in SOL
    // state (mirrors PHP's `handleTableCellTemplates` leading-`!` fixup).
    let prev_is_element_not_sol = prev_sibling.is_some_and(|p| {
        matches!(p.kind, NodeKind::Element(_)) && !puts_next_sibling_in_sol_state(p)
    });
    if cell_name == "th"
        && is_templated
        && reparse_type != ReparseScenario::MaybeCombineWithPrevCell
        && cell_tmp_flag(&cell, |t| t.table_cell_with_no_attribute_syntax)
        && prev_is_element_not_sol
        && let Some(first) = cell
            .children
            .iter_mut()
            .find(|c| matches!(c.kind, NodeKind::Text(_)) || matches!(c.kind, NodeKind::Element(_)))
        && let NodeKind::Text(t) = &mut first.kind
        && let Some(rest) = t.strip_prefix('!')
    {
        *t = rest.to_string();
        if let Some(dp) = cell.dp.as_mut() {
            dp.stx = Some("row".to_string());
            dp.tmp.non_mergeable_table_cell = true;
        }
    }

    if reparse_type == ReparseScenario::MaybeReparseAttrs
        && cell_tmp_flag(&cell, |t| t.table_cell_with_no_attribute_syntax)
    {
        let template_wrapper = if is_templated {
            Some(cell.clone())
        } else {
            None
        };
        reparse_templated_attributes(&mut cell, template_wrapper, config, source);
    }

    // Recurse into the cell's own children first (nested tables/cells).
    let children = std::mem::take(&mut cell.children);
    cell.children = process_children(children, config, source);

    // Split hidden cells on `||`/`!!`. Each new cell is a fresh `stx:row` cell
    // whose own `k=v|` prefix must also be reparsed, so recurse into each of them
    // (mirrors the DOMTraverser re-invoking `handleTableCellTemplates`).
    let mut split = split_hidden_cells(&mut cell, &cell_name, source);

    let mut out = Vec::with_capacity(1 + split.len());
    out.push(cell);
    for s in split.drain(..) {
        out.extend(process_cell(s, config, source, true, None));
    }
    out
}

/// `split_hidden_cells` — split a `<td>`/`<th>` on an embedded `||`/`!!`,
/// returning the newly created sibling cells (inserted after the cell).
///
/// Faithful to the tail of PHP `handleTableCellTemplates`: walk the children;
/// once a separator (`||` for `<td>`, or the shortest of `||`/`!!` for `<th>`)
/// is found in a text (or simple-templated-span) child, the child keeps the
/// prefix, a new `<td>`/`<th>` (`stx:row`, non-mergeable, no-attribute-syntax)
/// receives the suffix as its leading text, and every subsequent child is moved
/// into the new cell (where further separators split again).
fn split_hidden_cells(cell: &mut Node, cell_name: &str, source: Option<&str>) -> Vec<Node> {
    let is_td = cell_name == "td";

    let mut cell_children = std::mem::take(&mut cell.children);
    // Content bins: the first is the original cell's, subsequent bins are the
    // newly split cells' contents. `bin_abouts[i]` is the `about` id a split
    // cell (index `i - 1` in `bins`) inherited.
    let mut bins: Vec<Vec<Node>> = vec![Vec::new()];
    let mut new_abouts: Vec<Option<String>> = Vec::new();
    let orig_about = cell.get_attr("about").map(str::to_string);
    // `transclusions` mirrors PHP's `$transclusions`: every `mw:Transclusion`
    // child encountered while walking, cloned *before* any split mutates it.
    // `tpl_about` tracks the `about` of the current innermost transclusion so a
    // non-element child only participates in a split when it belongs to it.
    let mut transclusions: Vec<Node> = Vec::new();
    let mut tpl_about: Option<String> = None;
    let mut needs_tpl_hoist = false;

    for child in cell_children.drain(..) {
        let child_is_text = matches!(child.kind, NodeKind::Text(_));
        let child_is_simple_span = is_simple_templated_span(&child);

        // Track `mw:Transclusion` children (their `about` becomes the current
        // transclusion context), mirroring PHP's `$tplStart`/`$transclusions`.
        if has_type_of(&child, "mw:Transclusion") {
            tpl_about = child.get_attr("about").map(str::to_string);
            transclusions.push(child.clone());
        }

        if !child_is_text && !child_is_simple_span {
            bins.last_mut().expect("bin exists").push(child);
            continue;
        }

        let child_text = match &child.kind {
            NodeKind::Text(t) => t.clone(),
            _ => text_content(&child),
        };

        let found = if is_td {
            find_first_sep(&child_text, "||")
        } else {
            find_first_sep_th(&child_text)
        };

        let Some((prefix, suffix)) = found else {
            bins.last_mut().expect("bin exists").push(child);
            continue;
        };

        // The child keeps the prefix in the current bin. A simple templated
        // span stays a span (mirrors PHP, which sets `$child->textContent` to
        // the prefix and keeps the element); a text child just shrinks.
        let mut child = child;
        if child_is_simple_span {
            child.children = vec![Node::text(prefix.clone())];
        } else {
            child.kind = NodeKind::Text(prefix.clone());
        }
        bins.last_mut().expect("bin exists").push(child);

        // A new cell gets the suffix as its leading text.
        let mut new_bin: Vec<Node> = Vec::new();
        if !suffix.is_empty() {
            new_bin.push(Node::text(suffix));
        }
        bins.push(new_bin);

        // Mirror PHP's `$about` selection: a simple-span split hoists the
        // span's `about`, otherwise reuse the (possibly refreshed) cell about.
        let new_about = if child_is_simple_span {
            tpl_about.clone()
        } else {
            orig_about.clone()
        };
        if child_is_simple_span {
            needs_tpl_hoist = true;
        }
        new_abouts.push(new_about);
    }

    // Rebuild the original cell's children from bin[0].
    cell.children = bins.remove(0);

    // Build the new sibling cells from the remaining bins.
    let mut new_cells: Vec<Node> = Vec::with_capacity(bins.len());
    for (bin, about) in bins.into_iter().zip(new_abouts) {
        let mut new_cell = Node::element(if is_td {
            ElementKind::TableCell
        } else {
            ElementKind::TableHeader
        });
        new_cell.children = bin;
        new_cell.dp = Some(DataParsoid {
            stx: Some("row".to_string()),
            tmp: crate::wikitext::tokens_v2::TempData {
                table_cell_with_no_attribute_syntax: true,
                non_mergeable_table_cell: true,
                ..Default::default()
            },
            ..Default::default()
        });
        if let Some(a) = about {
            new_cell.set_attr("about", a);
        }
        new_cells.push(new_cell);
    }

    // Hoist transclusion info (typeof/about/data-mw) from the recorded
    // transclusions onto the original cell when a span-wrapper split occurred.
    // Using the recorded `transclusions` (not re-collecting) is essential: the
    // split already mutated the cell, and the span may no longer be discoverable.
    if needs_tpl_hoist && !transclusions.is_empty() {
        hoist_transclusion_info(cell, &transclusions, source);
    }

    new_cells
}

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

    #[test]
    fn test_split_hidden_cells_hoists_transclusion_on_th() {
        // `!a {{1x|b!!c}}` -> `<th>` with text "a " then an encapsulation span
        // wrapping "b!!c"; the `!!` split must create a second `<th>`, keep the
        // span for hoisting, and lift `typeof`/`about` onto the first `<th>`.
        let mut td = Node::element(ElementKind::TableHeader);
        td.push_child(Node::text("a "));
        let mut span = Node::element(ElementKind::Span);
        span.set_attr("typeof", "mw:Transclusion");
        span.set_attr("about", "#mwt1");
        span.push_child(Node::text("b!!c"));
        span.data_mw =
            Some("{\"parts\":[{\"template\":{\"target\":{\"wt\":\"1x\"}}}]}".to_string());
        span.dp = Some(DataParsoid {
            tmp: crate::wikitext::tokens_v2::TempData {
                wrapper: true,
                ..Default::default()
            },
            dsr: Some(crate::wikitext::tokens_v2::DomSourceRange {
                start: Some(3),
                end: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        });
        td.push_child(span);

        let new_cells = split_hidden_cells(&mut td, "th", None);

        // The original cell now carries the hoisted transclusion info.
        assert!(has_type_of(&td, "mw:Transclusion"), "{td:?}");
        assert_eq!(td.get_attr("about"), Some("#mwt1"));
        assert_eq!(text_content(&td), "a b");

        // The split cell keeps the suffix and shares the `about` id.
        assert_eq!(new_cells.len(), 1);
        assert_eq!(text_content(&new_cells[0]), "c");
        assert_eq!(new_cells[0].get_attr("about"), Some("#mwt1"));
    }
}
