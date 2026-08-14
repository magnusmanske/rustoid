//! ListHandler — faithful port of PHP Parsoid's `src/Wt2Html/TT/ListHandler.php`.
//!
//! Creates list tags around list items, mapping wiki bullet characters
//! (`*`, `#`, `;`, `:`) to HTML list/item tags (`<ul>/<ol>/<dl>` and
//! `<li>/<dt>/<dd>`).
//!
//! This is a line-based handler with a stack of `ListFrame`s (one per nested
//! table context).

use crate::wikitext::consts;
use crate::wikitext::tokens_v2::{
    DataParsoid, EndTagTk, Item, ListTk, ParsoidToken, SourceRange, TagTk,
};

/// Bullet character → (list tag, item tag) mapping. Mirrors PHP's static map.
fn bullet_map(bullet: char) -> Option<(&'static str, &'static str)> {
    match bullet {
        '*' => Some(("ul", "li")),
        '#' => Some(("ol", "li")),
        ';' => Some(("dl", "dt")),
        ':' => Some(("dl", "dd")),
        _ => None,
    }
}

/// A single list frame. Mirrors PHP's `ListFrame` class.
#[derive(Debug, Default, Clone)]
struct ListFrame {
    /// Flag indicating a list-less line that terminates a list block.
    at_eol: bool,
    /// NlTk that triggered at_eol.
    nl_tk: Option<Item>,
    sol_tokens: Vec<Item>,
    /// Bullet stack (previous element's list style).
    bstack: Vec<char>,
    /// Stack of end tags.
    endtags: Vec<ParsoidToken>,
    /// Number of open block tags in list context.
    num_open_block_tags: usize,
    /// Number of open tags in list context.
    num_open_tags: usize,
    /// Did we generate a <dd> already on this line?
    have_dd: bool,
    list_tk: ListTk,
}

impl ListFrame {
    fn new() -> Self {
        Self {
            at_eol: true,
            list_tk: ListTk::new(),
            ..Default::default()
        }
    }

    /// Pop `n` tags (list + item end tags).
    fn pop_tags(&mut self, n: usize) -> Vec<ParsoidToken> {
        let mut tokens = Vec::new();
        let mut remaining = n;
        while remaining > 0 {
            if let Some(t) = self.endtags.pop() {
                tokens.push(t);
            }
            if let Some(t) = self.endtags.pop() {
                tokens.push(t);
            }
            remaining -= 1;
        }
        tokens
    }

    /// Push a list open and item open, updating endtags and haveDD.
    fn push_list(
        &mut self,
        container: (&str, &str),
        dp1: DataParsoid,
        dp2: DataParsoid,
    ) -> Vec<ParsoidToken> {
        self.endtags.push(ParsoidToken::EndTag(EndTagTk::new(
            container.0,
            vec![],
            DataParsoid::default(),
        )));
        self.endtags.push(ParsoidToken::EndTag(EndTagTk::new(
            container.1,
            vec![],
            DataParsoid::default(),
        )));

        if container.1 == "dd" {
            self.have_dd = true;
        } else if container.1 == "dt" {
            self.have_dd = false;
        }

        if self.list_tk.list_type.is_none() {
            self.list_tk.list_type = Some(container.0.to_string());
        }

        vec![
            ParsoidToken::Tag(TagTk::new(container.0, vec![], dp1)),
            ParsoidToken::Tag(TagTk::new(container.1, vec![], dp2)),
        ]
    }
}

/// The ListHandler.
pub struct ListHandler {
    list_frame_stack: Vec<ListFrame>,
    nested_table_count: usize,
    in_t2529_mode: bool,
    have_active_list_frame: bool,
    on_any_enabled: bool,
}

impl ListHandler {
    pub fn new() -> Self {
        Self {
            list_frame_stack: Vec::new(),
            nested_table_count: 0,
            in_t2529_mode: false,
            have_active_list_frame: false,
            on_any_enabled: false,
        }
    }

    fn reset(&mut self) {
        self.list_frame_stack.clear();
        self.on_any_enabled = false;
        self.nested_table_count = 0;
        self.have_active_list_frame = false;
    }

    /// The HTML5 spec says certain closing tags generate implied list-item ends.
    fn generate_implied_end_tags(&self, tag_name: &str) -> bool {
        consts::wikitext_block_elems().contains(tag_name)
    }

    fn get_list_frame_mut(&mut self) -> &mut ListFrame {
        let idx = self.list_frame_stack.len() - 1;
        &mut self.list_frame_stack[idx]
    }

    /// Run the ListHandler over a token stream.
    pub fn run(&mut self, tokens: Vec<Item>) -> Vec<Item> {
        let mut output = Vec::new();
        let mut saw_eof = false;

        for token in tokens {
            if matches!(token, Item::Tok(ParsoidToken::Eof(_))) {
                saw_eof = true;
            }
            let res = self.on_token(token);
            if let Some(mut items) = res {
                output.append(&mut items);
            }
        }

        if !saw_eof
            && let Some(items) = self.on_end(Item::Tok(ParsoidToken::Eof(
                crate::wikitext::tokens_v2::EOFTk,
            )))
        {
            output.extend(items);
        }

        self.reset();
        output
    }

    /// Dispatch a token.
    fn on_token(&mut self, token: Item) -> Option<Vec<Item>> {
        match &token {
            Item::Tok(ParsoidToken::Tag(tk)) if tk.name == "listItem" => self.on_list_item(token),
            Item::Tok(ParsoidToken::Nl(_)) => self.on_newline(token),
            Item::Tok(ParsoidToken::Eof(_)) => self.on_end(token),
            Item::Tok(ParsoidToken::EmptyLine(_)) | Item::Tok(ParsoidToken::IndentPre(_)) => {
                // Compound tokens of no interest: nothing to do (pass through).
                Some(vec![token])
            }
            _ if self.on_any_enabled => self.on_any(token),
            _ => Some(vec![token]),
        }
    }

    /// Handle a listItem tag.
    fn on_list_item(&mut self, token: Item) -> Option<Vec<Item>> {
        if self.in_t2529_mode {
            if self.have_active_list_frame {
                self.get_list_frame_mut().have_dd = false;
            }
            self.in_t2529_mode = false;
        }

        self.on_any_enabled = true;

        let bullets_str = match &token {
            Item::Tok(ParsoidToken::Tag(tk)) => tk
                .attribs
                .iter()
                .find(|kv| kv.key.as_str() == Some("bullets"))
                .and_then(|kv| kv.value.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let bullets: Vec<char> = bullets_str.chars().collect();

        if self.have_active_list_frame {
            // Colon inside tags to prevent illegal overlapping.
            let last_is_colon = bullets.last() == Some(&':');
            let should_add_colon = {
                let list_frame = self.get_list_frame_mut();
                last_is_colon && (list_frame.have_dd || list_frame.num_open_tags > 0)
            };
            if should_add_colon {
                self.get_list_frame_mut()
                    .list_tk
                    .add_token(Item::Str(":".to_string()));
                return Some(Vec::new());
            }
        } else {
            let mut new_frame = ListFrame::new();
            new_frame.at_eol = false;
            self.list_frame_stack.push(new_frame);
            self.have_active_list_frame = true;
        }

        self.do_list_item(bullets, token)
    }

    /// Process a list item, emitting list/item tags.
    fn do_list_item(&mut self, bn: Vec<char>, token: Item) -> Option<Vec<Item>> {
        // Extract the token's DataParsoid for makeDP.
        let token_dp = match &token {
            Item::Tok(ParsoidToken::Tag(tk)) => tk.data_parsoid.clone(),
            _ => DataParsoid::default(),
        };

        // Get a mutable borrow of the top frame.
        let idx = self.list_frame_stack.len() - 1;
        let list_frame = &mut self.list_frame_stack[idx];

        let bs = list_frame.bstack.clone();
        let prefix_len = Self::common_prefix_length(&bs, &bn);
        let prefix: Vec<char> = bn[..prefix_len].to_vec();

        list_frame.bstack = bn.clone();

        let res: Vec<Item>;

        if prefix.len() == bs.len() && bn.len() == bs.len() {
            // No nesting change.
            let item_name = list_frame
                .endtags
                .pop()
                .map(|t| t.get_name().to_string())
                .unwrap_or_default();

            list_frame.endtags.push(ParsoidToken::EndTag(EndTagTk::new(
                item_name.clone(),
                vec![],
                DataParsoid::default(),
            )));

            let item_open = ParsoidToken::Tag(TagTk::new(
                item_name.clone(),
                vec![],
                Self::make_dp(&token_dp, 0, bn.len()),
            ));

            let mut out = vec![Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                item_name.clone(),
                vec![],
                DataParsoid::default(),
            )))];
            out.extend(list_frame.sol_tokens.iter().cloned());
            if let Some(nl) = list_frame.nl_tk.clone() {
                out.push(nl);
            }
            out.push(Item::Tok(item_open));

            res = out;
        } else {
            let mut prefix_correction = 0usize;
            let mut tokens: Vec<Item> = Vec::new();

            if bs.len() > prefix_len
                && bn.len() > prefix_len
                && Self::is_dt_dd(bs[prefix_len], bn[prefix_len])
            {
                // dt/dd transition.
                let pop_n = bs.len() - prefix_len - 1;
                let popped = list_frame.pop_tags(pop_n);
                tokens.extend(list_frame.sol_tokens.iter().cloned());
                tokens.extend(popped.into_iter().map(Item::Tok));

                let (_, new_item_name) = bullet_map(bn[prefix_len]).unwrap();
                let end_tag_name = list_frame
                    .endtags
                    .pop()
                    .map(|t| t.get_name().to_string())
                    .unwrap_or_default();

                if new_item_name == "dd" {
                    list_frame.have_dd = true;
                } else if new_item_name == "dt" {
                    list_frame.have_dd = false;
                }

                list_frame.endtags.push(ParsoidToken::EndTag(EndTagTk::new(
                    new_item_name,
                    vec![],
                    DataParsoid::default(),
                )));

                let is_row = token_dp.stx.as_deref() == Some("row");
                let new_tag_dp = if is_row {
                    Self::make_dp(&token_dp, 0, 1)
                } else {
                    Self::make_dp(&token_dp, 0, prefix_len + 1)
                };

                tokens.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                    end_tag_name,
                    vec![],
                    DataParsoid::default(),
                ))));
                if let Some(nl) = list_frame.nl_tk.clone() {
                    tokens.push(nl);
                }
                tokens.push(Item::Tok(ParsoidToken::Tag(TagTk::new(
                    new_item_name,
                    vec![],
                    new_tag_dp,
                ))));

                prefix_correction = 1;
            } else {
                // Reduced nesting.
                let pop_n = bs.len() - prefix_len;
                let popped = list_frame.pop_tags(pop_n);
                tokens.extend(list_frame.sol_tokens.iter().cloned());
                tokens.extend(popped.into_iter().map(Item::Tok));

                if let Some(nl) = list_frame.nl_tk.clone() {
                    tokens.push(nl);
                }

                if prefix_len > 0 && bn.len() == prefix_len {
                    let item_name = list_frame
                        .endtags
                        .pop()
                        .map(|t| t.get_name().to_string())
                        .unwrap_or_default();
                    tokens.push(Item::Tok(ParsoidToken::EndTag(EndTagTk::new(
                        item_name.clone(),
                        vec![],
                        DataParsoid::default(),
                    ))));
                    tokens.push(Item::Tok(ParsoidToken::Tag(TagTk::new(
                        item_name.clone(),
                        vec![],
                        Self::make_dp(&token_dp, 0, bn.len()),
                    ))));
                    list_frame.endtags.push(ParsoidToken::EndTag(EndTagTk::new(
                        item_name.clone(),
                        vec![],
                        DataParsoid::default(),
                    )));
                }
            }

            for (i, c) in bn.iter().enumerate().skip(prefix_len + prefix_correction) {
                let Some(container) = bullet_map(*c) else {
                    // Unknown bullet; PHP throws. We skip (return None).
                    // Reset sol tokens/nl/atEOL first.
                    list_frame.sol_tokens.clear();
                    list_frame.nl_tk = None;
                    list_frame.at_eol = false;
                    self.have_active_list_frame = false;
                    return Some(Vec::new());
                };

                let (list_dp, item_dp) = if i == prefix_len {
                    (
                        Self::make_dp(&token_dp, 0, 0),
                        Self::make_dp(&token_dp, 0, i + 1),
                    )
                } else {
                    (
                        Self::make_dp(&token_dp, i, i),
                        Self::make_dp(&token_dp, i, i + 1),
                    )
                };

                let pushed = list_frame.push_list(container, list_dp, item_dp);
                tokens.extend(pushed.into_iter().map(Item::Tok));
            }

            res = tokens;
        }

        // Clear out sol tokens, nl, atEOL.
        list_frame.sol_tokens.clear();
        list_frame.nl_tk = None;
        list_frame.at_eol = false;
        list_frame.list_tk.add_tokens(res);

        // Reborrow ends. Return empty (tokens buffered in list frame).
        Some(Vec::new())
    }

    /// Handle a newline token.
    fn on_newline(&mut self, token: Item) -> Option<Vec<Item>> {
        if !self.on_any_enabled {
            return Some(vec![token]);
        }

        let idx = self.list_frame_stack.len() - 1;
        if !self.have_active_list_frame {
            self.list_frame_stack[idx].list_tk.add_token(token);
            return Some(Vec::new());
        }

        let list_frame = &mut self.list_frame_stack[idx];
        if list_frame.at_eol {
            // Non-list item in newline context → close all lists.
            return self.close_lists(idx, Some(token));
        }

        list_frame.at_eol = true;
        list_frame.nl_tk = Some(token);
        list_frame.have_dd = false;
        list_frame.num_open_tags = 0;
        Some(Vec::new())
    }

    /// Handle any other (non-listItem/newline/EOF) token.
    fn on_any(&mut self, token: Item) -> Option<Vec<Item>> {
        // T2529 detection: transclusion token resets inT2529Mode.
        if let Item::Tok(ParsoidToken::SelfclosingTag(tk)) = &token {
            let is_transclusion = tk.attribs.iter().any(|kv| {
                kv.key.as_str() == Some("typeof") && kv.value.as_str() == Some("mw:Transclusion")
            });
            if is_transclusion {
                self.in_t2529_mode = false;
            }
        }
        // Non-SOL-transparent resets T2529 mode.
        if !Self::is_sol_transparent(&token) {
            self.in_t2529_mode = false;
        }

        let idx = self.list_frame_stack.len() - 1;

        if !self.have_active_list_frame {
            // Handle table start/end while not in a list.
            let is_table_open =
                matches!(&token, Item::Tok(ParsoidToken::Tag(t)) if t.name == "table");
            let is_table_close =
                matches!(&token, Item::Tok(ParsoidToken::EndTag(t)) if t.name == "table");

            if is_table_close {
                if self.nested_table_count == 0 {
                    self.have_active_list_frame = true;
                } else {
                    self.nested_table_count -= 1;
                }
            } else if is_table_open {
                self.nested_table_count += 1;
            }

            self.list_frame_stack[idx].list_tk.add_token(token);
            return Some(Vec::new());
        }

        // Track open tags.
        if let Item::Tok(ParsoidToken::Tag(t)) = &token {
            if t.name != "table" {
                self.list_frame_stack[idx].num_open_tags += 1;
            }
        } else if let Item::Tok(ParsoidToken::EndTag(t)) = &token {
            let frame = &mut self.list_frame_stack[idx];
            if frame.num_open_tags > 0 {
                frame.num_open_tags -= 1;
            }

            if t.name == "table" {
                // Close all open lists and pop a frame.
                let ret = self.close_lists(idx, Some(token.clone()));
                if !self.list_frame_stack.is_empty() {
                    self.have_active_list_frame = true;
                }
                return ret;
            } else if self.generate_implied_end_tags(&t.name) {
                let frame = &mut self.list_frame_stack[idx];
                if frame.num_open_block_tags == 0 {
                    return self.close_lists(idx, Some(token));
                } else {
                    frame.num_open_block_tags -= 1;
                    if frame.at_eol {
                        return self.close_lists(idx, Some(token));
                    } else {
                        frame.list_tk.add_token(token);
                        return Some(Vec::new());
                    }
                }
            }
        }

        // atEOL handling.
        let frame = &mut self.list_frame_stack[idx];
        if frame.at_eol {
            if Self::is_sol_transparent(&token) {
                if frame.nl_tk.is_some() {
                    let nl = frame.nl_tk.take().unwrap();
                    frame.sol_tokens.push(nl);
                }
                frame.sol_tokens.push(token);
                return Some(Vec::new());
            } else {
                return self.close_lists(idx, Some(token));
            }
        }

        // Non-block tag handling.
        if let Item::Tok(ParsoidToken::Tag(t)) = &token {
            if t.name == "table" {
                self.have_active_list_frame = false;
            } else if self.generate_implied_end_tags(&t.name) {
                self.list_frame_stack[idx].num_open_block_tags += 1;
            }
        }

        self.list_frame_stack[idx].list_tk.add_token(token);
        Some(Vec::new())
    }

    /// Handle EOF.
    fn on_end(&mut self, token: Item) -> Option<Vec<Item>> {
        let mut toks = if self.have_active_list_frame {
            let idx = self.list_frame_stack.len() - 1;
            // close all lists without passing the token.
            let ret = self.close_lists(idx, None);
            ret.unwrap_or_default()
        } else {
            Vec::new()
        };

        while !self.list_frame_stack.is_empty() {
            let idx = self.list_frame_stack.len() - 1;
            let list_tk = self.list_frame_stack[idx].list_tk.clone();
            toks = self.pop_list_frame(vec![Item::Tok(ParsoidToken::List(list_tk))]);
        }

        toks.push(token);
        Some(toks)
    }

    /// Close lists for the frame at `idx`, returning tokens.
    fn close_lists(&mut self, idx: usize, token: Option<Item>) -> Option<Vec<Item>> {
        let list_frame = &mut self.list_frame_stack[idx];
        let pop_n = list_frame.bstack.len();
        let popped = list_frame.pop_tags(pop_n);
        list_frame
            .list_tk
            .add_tokens(popped.into_iter().map(Item::Tok).collect());

        let mut ret = vec![Item::Tok(ParsoidToken::List(list_frame.list_tk.clone()))];
        ret.extend(list_frame.sol_tokens.iter().cloned());
        if let Some(nl) = list_frame.nl_tk.clone() {
            ret.push(nl);
        }
        if let Some(token) = token {
            ret.push(token);
        }

        self.pop_list_frame(ret).into()
    }

    /// Pop the top list frame, appending tokens to the parent frame if any.
    fn pop_list_frame(&mut self, ret: Vec<Item>) -> Vec<Item> {
        self.list_frame_stack.pop();
        self.have_active_list_frame = false;

        if !self.list_frame_stack.is_empty() {
            let idx = self.list_frame_stack.len() - 1;
            self.list_frame_stack[idx].list_tk.add_tokens(ret);
            Vec::new()
        } else {
            self.on_any_enabled = false;
            ret
        }
    }

    /// Common prefix length of two char slices.
    fn common_prefix_length(x: &[char], y: &[char]) -> usize {
        let min_len = x.len().min(y.len());
        let mut i = 0;
        while i < min_len && x[i] == y[i] {
            i += 1;
        }
        i
    }

    /// Check for a dt/dd transition (`:` and `;` in either order).
    fn is_dt_dd(a: char, b: char) -> bool {
        (a == ':' && b == ';') || (a == ';' && b == ':')
    }

    /// Make a DataParsoid that is a slice of a source DataParsoid's tsr.
    fn make_dp(source_dp: &DataParsoid, start_offset: usize, end_offset: usize) -> DataParsoid {
        let mut new_dp = source_dp.clone();
        if let Some(tsr) = &source_dp.tsr {
            new_dp.tsr = Some(SourceRange::new(
                tsr.start + start_offset,
                tsr.start + end_offset,
            ));
        }
        new_dp
    }

    /// Is a token sol-transparent?
    fn is_sol_transparent(item: &Item) -> bool {
        match item {
            Item::Str(s) => !s.is_empty() && s.chars().all(|c| c == ' ' || c == '\t'),
            Item::Tok(ParsoidToken::Comment(_)) => true,
            Item::Tok(ParsoidToken::EmptyLine(_)) => true,
            Item::Tok(ParsoidToken::SelfclosingTag(tk)) if tk.name == "behavior-switch" => true,
            _ => false,
        }
    }
}

impl Default for ListHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_item(bullets: &str) -> Item {
        let mut tk = TagTk::new("listItem", vec![], DataParsoid::default());
        tk.add_attribute_str("bullets", bullets);
        Item::Tok(ParsoidToken::Tag(tk))
    }

    fn nl() -> Item {
        Item::Tok(ParsoidToken::Nl(crate::wikitext::tokens_v2::NlTk::new(
            SourceRange::new(0, 1),
        )))
    }

    fn eof() -> Item {
        Item::Tok(ParsoidToken::Eof(crate::wikitext::tokens_v2::EOFTk))
    }

    #[test]
    fn test_simple_bullet_list() {
        // "* a\n* b" → <ul><li>a</li><li>b</li></ul>
        let mut handler = ListHandler::new();
        let out = handler.run(vec![
            list_item("*"),
            Item::Str("a".to_string()),
            nl(),
            list_item("*"),
            Item::Str("b".to_string()),
            nl(),
            eof(),
        ]);

        // Should contain a <ul> list token with nested <li>.
        let has_list = out
            .iter()
            .any(|it| matches!(it, Item::Tok(ParsoidToken::List(_))));
        assert!(has_list, "expected list token in {:?}", out);
    }

    #[test]
    fn test_empty_input() {
        let mut handler = ListHandler::new();
        let out = handler.run(vec![eof()]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Item::Tok(ParsoidToken::Eof(_))));
    }
}
