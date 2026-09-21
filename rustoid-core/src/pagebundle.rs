//! Page-bundle node ids: the `id="mwAQ"`-style attributes on Parsoid output.
//!
//! A live wiki serves Parsoid HTML in **page-bundle** mode, where nearly every
//! element carries an `id` that keys it in the bundle's `parsoid`/`mw` maps. The
//! ids are not content-derived; they are a counter, and reproducing them exactly
//! requires reproducing three things: the encoding, which elements consume a
//! counter, and the traversal order.
//!
//! The algorithm is Parsoid's `Utils\CounterType::counterToId` with
//! `CounterType::NODE_DATA_ID`. Read from the source rather than inferred, because
//! the encoding is not what it looks like: the counter is written as **big-endian
//! bytes and then base64-encoded**, not as base-62 digits.
//!
//! ```php
//! private static function counterToBase64( int $n ): string {
//!     $str = '';
//!     do { $str = chr( $n & 0xff ) . $str; $n >>= 8; } while ( $n > 0 );
//!     return rtrim( strtr( base64_encode( $str ), '+/', '-_' ), '=' );
//! }
//! ```
//!
//! So counter 1 is the byte `0x01`, base64 `AQ`, giving `mwAQ`; counter 4 is
//! `0x04` → `BA` → `mwBA`. The visible cycle `AQ, Ag, Aw, BA, …` is base64's
//! alphabet, *not* a base-conversion carry. Guessing base-62 reproduces the first
//! 63 ids and then diverges silently — which is exactly the kind of bug that looks
//! like a near-miss forever.
//!
//! Three rules from the same source, each of which shifts every later id if got
//! wrong:
//!
//! 1. **Only some elements get an id.** Parsoid assigns one to a node that has
//!    `data-parsoid` and/or `data-mw` *to store*, i.e. metadata it wants to move
//!    into the bundle. A node with neither must not consume a counter.
//! 2. **An element that already has an id keeps it**, and does not consume a
//!    counter — unless that id collides with one in the bundle's `ids` map, in
//!    which case the original is moved to `data-x-id` and a fresh id is generated.
//! 3. **Pre-order depth-first over the body**, sub-trees included, starting at
//!    counter 1 (the counter is initialised to `-1` and pre-incremented, so
//!    `mwAA` is never emitted).
//!
//! Rule 3's *retry* is the subtle one: the allocation loop is
//! `do { counter += 1; id = encode(counter); } while (isset($idIndex[$id]))`, so a
//! counter whose id collides with one already present is **skipped**, leaving a
//! hole in the sequence. A real page shows these holes — `Zebra` jumps from
//! counter 434 to 436 at one point — so an implementation that assumes a dense
//! `1..n` will drift at the first collision and never recover.

/// Encodes counters into Parsoid's `mw`-prefixed node ids.
///
/// Mirrors `CounterType::NODE_DATA_ID->counterToId`.
///
/// The maximum is `i64::MAX` rather than PHP's 32-bit limit: PHP's warning about
/// "Max integer is 2^31 - 1 for bitwise operations" describes its own `>>`, and
/// nothing in the algorithm needs the bound. Document ids are far below either.
pub fn counter_to_id(counter: u64) -> String {
    if counter == 0 {
        // Counter 0 would encode as the byte 0x00, base64 "AA", giving `mwAA`.
        // Parsoid never emits it (the counter starts at -1 and is pre-incremented),
        // so returning it here would be inventing an id. Callers start at 1.
        return "mwAA".to_string();
    }
    // Big-endian bytes, minimum one.
    let mut bytes = Vec::new();
    let mut n = counter;
    while n > 0 {
        bytes.push((n & 0xff) as u8);
        n >>= 8;
    }
    bytes.reverse();
    format!("mw{}", base64_url_nopad(&bytes))
}

/// Standard base64 with `+`/`/` mapped to `-`/`_` and padding stripped.
fn base64_url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        // The number of output characters the input length implies: 3 bytes -> 4,
        // 2 -> 3, 1 -> 2. That is exactly the padding rule, so no `=` is written.
        let count = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for i in 0..count {
            let idx = ((n >> (18 - 6 * i)) & 0x3f) as usize;
            let c = ALPHABET[idx] as char;
            out.push(match c {
                '+' => '-',
                '/' => '_',
                c => c,
            });
        }
    }
    out
}

/// Recovers the counter from an id, or `None` if `id` is not one.
///
/// The inverse is useful for tests and diagnostics — "which counter does this
/// element have" answers "how many metadata-bearing elements came before it".
pub fn id_to_counter(id: &str) -> Option<u64> {
    let encoded = id.strip_prefix("mw")?;
    if encoded.is_empty() {
        return None;
    }
    // Reject the other id families that share the `mw` prefix. An annotation
    // `about` id is `mwa<digits>` and a transclusion `about` id is `#mwt<digits>`
    // (the `#` is stripped by the time it reaches an attribute), so both are
    // `mw` + a letter + digits. Neither encodes a counter in base64, so decoding
    // one would quietly return a number that means nothing.
    //
    // The test is on the *digit suffix*, not on the one leading letter: `mwAbQ`
    // is a legitimate node id whose second character happens to be a letter.
    if let Some(rest) = encoded
        .strip_prefix('a')
        .or_else(|| encoded.strip_prefix('t'))
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4 + 1);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in encoded.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' => 62,
            '_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bytes.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for b in &bytes {
        n = (n << 8) | *b as u64;
    }
    Some(n)
}

/// Allocates node ids across one document, mirroring Parsoid's page-bundle pass.
///
/// The counter and the set of ids already present in the document live together
/// because they interact: a generated id that collides with an existing one is
/// skipped, so the allocator has to know about the source's own ids before it
/// starts.
#[derive(Debug)]
pub struct NodeIdAllocator {
    counter: u64,
    taken: std::collections::HashSet<String>,
}

impl NodeIdAllocator {
    /// An allocator that will skip the given pre-existing ids.
    ///
    /// Every `id` attribute present in the document before the pass goes here,
    /// including ones Parsoid would normally keep — a later element may generate
    /// one that collides, and Parsoid's loop tests against the whole index.
    pub fn new(existing: impl IntoIterator<Item = String>) -> Self {
        Self {
            counter: 0,
            taken: existing.into_iter().collect(),
        }
    }

    /// The next id, advancing past any collision.
    ///
    /// The first call returns `mwAQ` (counter 1), because Parsoid initialises the
    /// counter to `-1` and increments before use.
    pub fn next_id(&mut self) -> String {
        loop {
            self.counter += 1;
            let id = counter_to_id(self.counter);
            if !self.taken.contains(&id) {
                self.taken.insert(id.clone());
                return id;
            }
        }
    }

    /// How many counters have been consumed, collisions included.
    pub fn counter(&self) -> u64 {
        self.counter
    }
}

/// Assign page-bundle node ids to a built DOM, in place.
///
/// Mirrors `DOMDataUtils::visitAndStoreDataAttribs`' id allocation. It runs at the
/// very end of the pipeline, after every pass that can create or destroy elements,
/// because the ids are positional: an element inserted earlier shifts every id
/// after it.
///
/// The three rules are the ones above, and each is load-bearing:
///
/// - **Only metadata-bearing elements take an id.** The test is `data-parsoid` or
///   `data-mw` being present, which is the marker for "Parsoid has something to key
///   this node by". An element with neither must not consume a counter, or every
///   later id shifts.
/// - **An element that already has an `id` keeps it** and takes no counter. The
///   exception in Parsoid is a collision, which is handled here by seeding the
///   allocator with the document's existing ids: a *generated* id that happens to
///   collide is skipped, leaving the hole a real page shows.
/// - **Pre-order depth-first from the body.** The body itself is visited first,
///   but carries no metadata, so it takes no id.
///
/// Returns the number of ids assigned, which is what a caller wants to log or
/// assert; the ids themselves are written into the tree.
pub fn assign_node_ids(root: &mut crate::dom::node::Node) -> usize {
    // Rule 2, second half: the allocator must know every id already in the tree
    // before it hands out any, because a generated id that collides with one of
    // those is skipped. Collecting first, then assigning, keeps the two phases from
    // interfering — an id assigned early would otherwise be indistinguishable from
    // one that was always there.
    let mut existing = Vec::new();
    collect_ids(root, &mut existing);
    let mut alloc = NodeIdAllocator::new(existing);
    assign_walk(root, &mut alloc)
}

/// Collect every `id` attribute already present, in document order.
fn collect_ids(node: &crate::dom::node::Node, out: &mut Vec<String>) {
    if let Some(id) = node.get_attr("id")
        && !id.is_empty()
    {
        out.push(id.to_string());
    }
    for child in &node.children {
        collect_ids(child, out);
    }
}

/// Pre-order walk, assigning an id where the rules call for one.
fn assign_walk(node: &mut crate::dom::node::Node, alloc: &mut NodeIdAllocator) -> usize {
    let mut assigned = 0;

    if node.kind.is_element() {
        // A node takes an id when Parsoid has something to key it by. That is
        // `data-parsoid` or `data-mw`, plus the elements that were given an empty
        // `data-parsoid` by `serializeNewEmptyDp` — [`Node::empty_dp_slot`]. An
        // empty `id=""` is treated as absent, as Parsoid does ("Forcibly reset
        // the ID if it is invalid").
        let has_metadata =
            node.data_parsoid.is_some() || node.data_mw.is_some() || node.empty_dp_slot;
        let has_id = node.get_attr("id").is_some_and(|v| !v.is_empty());
        if has_metadata && !has_id {
            let id = alloc.next_id();
            node.set_attr("id", id);
            assigned += 1;
        }
    }

    for child in &mut node.children {
        assigned += assign_walk(child, alloc);
    }
    assigned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 28 ids of a real page, copied from the cached Parsoid output of
    /// `Zebra` (`rustoid-compare`'s corpus cache).
    ///
    /// This is the assertion that matters: the encoding is counter-intuitive
    /// enough that a wrong implementation still looks plausible.
    #[test]
    fn the_first_ids_match_a_real_page() {
        const REAL: &[&str] = &[
            "mwAQ", "mwAg", "mwAw", "mwBA", "mwBQ", "mwBg", "mwBw", "mwCA", "mwCQ", "mwCg", "mwCw",
            "mwDA", "mwDQ", "mwDg", "mwDw", "mwEA", "mwEQ", "mwEg", "mwEw", "mwFA", "mwFQ", "mwFg",
            "mwFw", "mwGA", "mwGQ", "mwGg", "mwGw", "mwHA",
        ];
        let got: Vec<String> = (1..=REAL.len() as u64).map(counter_to_id).collect();
        assert_eq!(got, REAL);
    }

    /// The `A, Q, g, w` cycle is base64's alphabet, so the ids are the base64 of
    /// the counter's bytes. Spelling this out because a base-62 implementation
    /// passes the test above and then diverges at 64.
    #[test]
    fn encoding_is_base64_of_the_counters_bytes() {
        assert_eq!(counter_to_id(1), "mwAQ");
        assert_eq!(counter_to_id(4), "mwBA");
        // 64 is the first counter needing a second byte: 0x40 -> base64 "QA".
        assert_eq!(counter_to_id(64), "mwQA");
        // 255 is one byte of 0xff -> "/w" -> url-safe "_w".
        assert_eq!(counter_to_id(255), "mw_w");
        // 256 needs two bytes: 0x01 0x00 -> "AQA".
        assert_eq!(counter_to_id(256), "mwAQA");
    }

    /// Base-62 would give a different id at 64, which is where the two first
    /// disagree — pinned so a rewrite to base-62 fails loudly rather than subtly.
    #[test]
    fn the_encoding_is_not_base62() {
        // Under base-62 (`A-Za-z0-9`) counter 64 would be "mwB2".
        assert_ne!(
            counter_to_id(64),
            "mwB2",
            "base-62 must not be reintroduced"
        );
    }

    #[test]
    fn ids_round_trip_to_their_counter() {
        for n in [1u64, 4, 63, 64, 255, 256, 2959, 100_000, 1 << 40] {
            let id = counter_to_id(n);
            assert_eq!(id_to_counter(&id), Some(n), "{id} should decode to {n}");
        }
    }

    /// The other id families share the `mw` prefix but are not counter-encoded.
    #[test]
    fn annotation_and_transclusion_ids_are_rejected() {
        assert_eq!(id_to_counter("mwa0"), None);
        assert_eq!(id_to_counter("mwt1"), None);
        assert_eq!(id_to_counter("#mwt1"), None);
        assert_eq!(id_to_counter("mw"), None);
        assert_eq!(id_to_counter("cite_ref-x_1-0"), None);
    }

    #[test]
    fn the_first_allocated_id_is_one_not_zero() {
        let mut a = NodeIdAllocator::new([]);
        assert_eq!(a.next_id(), "mwAQ");
        assert_eq!(a.next_id(), "mwAg");
        assert_eq!(a.counter(), 2);
    }

    /// A generated id that collides with one already in the document is skipped,
    /// leaving a hole — which is what a real page shows.
    ///
    /// `Zebra` jumps from counter 434 to 436, so an implementation assuming a
    /// dense sequence drifts at the first collision and never recovers.
    #[test]
    fn a_collision_is_skipped_and_leaves_a_hole() {
        let mut a = NodeIdAllocator::new(["mwAg".to_string()]);
        assert_eq!(a.next_id(), "mwAQ");
        // `mwAg` is taken, so counter 2 is consumed without being handed out.
        assert_eq!(a.next_id(), "mwAw");
        assert_eq!(a.counter(), 3, "the skipped counter is still advanced");
    }

    // ---- the DOM pass ----

    use crate::dom::node::{ElementKind, Node};

    /// An element carrying metadata, which is what draws an id.
    fn storable() -> Node {
        let mut n = Node::element(ElementKind::Other("span".to_string()));
        n.data_parsoid = Some("{}".to_string());
        n
    }

    /// An element with no metadata to key, which must not consume a counter.
    fn plain() -> Node {
        Node::element(ElementKind::Other("span".to_string()))
    }

    #[test]
    fn ids_are_assigned_in_document_order() {
        let mut root = Node::document();
        root.push_child(storable());
        root.push_child(storable());
        let n = assign_node_ids(&mut root);
        assert_eq!(n, 2);
        assert_eq!(root.children[0].get_attr("id"), Some("mwAQ"));
        assert_eq!(root.children[1].get_attr("id"), Some("mwAg"));
    }

    /// Rule 1: an element with no `data-parsoid`/`data-mw` takes no counter.
    ///
    /// Getting this wrong is not a cosmetic slip — it shifts the id of every
    /// element after it.
    #[test]
    fn an_element_without_metadata_takes_no_id() {
        let mut root = Node::document();
        root.push_child(storable());
        root.push_child(plain());
        root.push_child(storable());
        assign_node_ids(&mut root);
        assert_eq!(root.children[0].get_attr("id"), Some("mwAQ"));
        assert_eq!(root.children[1].get_attr("id"), None);
        assert_eq!(
            root.children[2].get_attr("id"),
            Some("mwAg"),
            "the plain element must not have consumed counter 2"
        );
    }

    /// Rule 2: an existing id is kept and consumes no counter.
    #[test]
    fn an_existing_id_is_preserved() {
        let mut root = Node::document();
        let mut first = storable();
        first.set_attr("id", "cite_note-x-1");
        root.push_child(first);
        root.push_child(storable());
        assign_node_ids(&mut root);
        assert_eq!(root.children[0].get_attr("id"), Some("cite_note-x-1"));
        // The second element still gets counter 1, because the first took none.
        assert_eq!(root.children[1].get_attr("id"), Some("mwAQ"));
    }

    /// An empty `id=""` is invalid and is replaced, as Parsoid does.
    #[test]
    fn an_empty_id_is_replaced() {
        let mut root = Node::document();
        let mut n = storable();
        n.set_attr("id", "");
        root.push_child(n);
        assign_node_ids(&mut root);
        assert_eq!(root.children[0].get_attr("id"), Some("mwAQ"));
    }

    /// Rule 3: the walk is depth-first, so a child is numbered before its
    /// following sibling — and before the sibling's own subtree.
    #[test]
    fn the_walk_is_depth_first() {
        let mut root = Node::document();
        let mut outer = plain();
        outer.push_child(storable()); // -> mwAQ, as the first metadata-bearing node
        root.push_child(outer);
        root.push_child(storable()); // -> mwAg, only after the subtree above
        assign_node_ids(&mut root);
        assert_eq!(root.children[0].children[0].get_attr("id"), Some("mwAQ"));
        assert_eq!(root.children[1].get_attr("id"), Some("mwAg"));
    }

    /// A generated id colliding with one already in the document is skipped, which
    /// is what leaves a hole in a real page's sequence.
    #[test]
    fn a_collision_with_an_existing_id_leaves_a_hole() {
        let mut root = Node::document();
        // Something in the document already owns the id counter 2 would get.
        let mut squatter = plain();
        squatter.set_attr("id", "mwAg");
        root.push_child(squatter);
        root.push_child(storable());
        root.push_child(storable());
        assign_node_ids(&mut root);
        assert_eq!(root.children[1].get_attr("id"), Some("mwAQ"));
        assert_eq!(
            root.children[2].get_attr("id"),
            Some("mwAw"),
            "counter 2 is skipped because its id is taken"
        );
    }

    /// Text and comments cannot hold attributes, so the walk must not try.
    #[test]
    fn non_elements_are_skipped() {
        let mut root = Node::document();
        root.push_child(Node::text("hello"));
        root.push_child(Node::comment("c"));
        root.push_child(storable());
        assert_eq!(assign_node_ids(&mut root), 1);
        assert_eq!(root.children[2].get_attr("id"), Some("mwAQ"));
    }
}
