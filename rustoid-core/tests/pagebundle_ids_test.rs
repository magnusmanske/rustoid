//! The node-id allocator against a real page's Parsoid output.
//!
//! The unit tests pin the encoding. This one checks the part that cannot be
//! checked from the algorithm alone: that a **whole real document's** id sequence
//! is reproduced, holes included.
//!
//! `Zebra` is used because it is cached in `rustoid-compare`'s corpus cache, so
//! the expectation is the wiki's own output rather than a hand-written fixture.
//! The test skips (rather than fails) when the cache is absent, because the cache
//! is a local artifact and CI has no network — see "golden files" in
//! `ONLINE-PARITY.md`.

use rustoid_core::pagebundle::{counter_to_id, id_to_counter};

/// The cached Parsoid HTML for `Zebra`, if this machine has one.
fn cached_zebra() -> Option<String> {
    let root = std::env::var_os("RUSTOID_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|h| h.join(".cache").join("rustoid-compare"))
        })?;
    let candidates = [
        root.join("en.wikipedia.org")
            .join("pages")
            .join("html__Zebra.txt"),
        root.join("en.wikipedia.org")
            .join("pages")
            .join("html:Zebra.txt"),
    ];
    candidates
        .into_iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
}

/// Whether `id` is one the node allocator could have generated.
///
/// The base64 alphabet admits a vast space of strings that *look* generated — the
/// page contains `mw-reference-text-cite_note-61`, which decodes as perfectly
/// valid base64, has a legal length, and even re-encodes to itself. Guessing from
/// the alphabet is not enough.
///
/// Two properties together do separate them, and both follow from the encoder
/// emitting the **minimum** number of bytes for a counter:
///
/// - re-encoding what was decoded must return the same string (the minimal-byte
///   property), and
/// - the byte length must be one a document could plausibly reach. Ids are
///   allocated one per metadata-bearing element, so a real page tops out in the
///   thousands — two bytes, occasionally three. `mw-reference-text-cite_note-61`
///   decodes to 21 bytes, which would need ~10^50 elements.
///
/// The bound is on *bytes* rather than on a count, so it stays honest about what it
/// is: `2^16` counters is already more elements than any page has.
fn is_generated_id(id: &str) -> bool {
    let Some(n) = id_to_counter(id) else {
        return false;
    };
    if n == 0 || counter_to_id(n) != id {
        return false;
    }
    // The counter must be one a page could plausibly reach. Ids are allocated one
    // per metadata-bearing element, so a real page tops out in the thousands —
    // two bytes, occasionally three. `mw-reference-text-cite_note-61` decodes to
    // 21 bytes, which would need ~10^50 elements.
    //
    // The cap is on bytes rather than a raw count so that it stays honest about
    // what it is: `2^24` counters is already more elements than any page has.
    n < (1 << 24)
}

/// Every generated `id="mw…"` attribute in document order.
fn node_ids(html: &str) -> Vec<String> {
    let body = match (html.find("<body"), html.rfind("</body>")) {
        (Some(b), Some(e)) if b < e => &html[b..e],
        _ => html,
    };
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find("id=\"mw") {
        let after = &rest[pos + 4..];
        let end = after.find('"').unwrap_or(0);
        let value = &after[..end];
        if is_generated_id(value) {
            out.push(value.to_string());
        }
        rest = &after[end..];
    }
    out
}

/// The allocator must reproduce a real page's id sequence exactly, holes included.
///
/// The interesting property is not that it counts, but that it **skips** counters
/// whose id collides with one already in the document. `Zebra` has exactly two
/// such holes — 434→436 and 3026→3032 — so an implementation assuming a dense
/// sequence matches the first 434 ids and then drifts for the remaining 2600.
///
/// The whole sequence is asserted, not a sample: the ids are a running counter, so
/// one wrong step invalidates every id after it, and a spot check would miss a
/// divergence in the middle.
#[test]
fn a_real_pages_id_sequence_is_reproduced() {
    let Some(html) = cached_zebra() else {
        eprintln!("skipped: no cached Parsoid HTML for Zebra");
        return;
    };
    let real = node_ids(&html);
    assert!(
        real.len() > 1000,
        "expected a substantial page, got {} ids",
        real.len()
    );

    let counters: Vec<u64> = real.iter().filter_map(|i| id_to_counter(i)).collect();
    assert_eq!(counters.len(), real.len(), "every id must decode");

    // Strictly increasing, and never by more than the collision loop can explain.
    let mut holes = Vec::new();
    for pair in counters.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        assert!(b > a, "ids must strictly increase: {a} then {b}");
        if b != a + 1 {
            holes.push((a, b));
        }
    }

    // The first id is counter 1 (`mwAQ`), because Parsoid initialises the counter
    // to -1 and pre-increments; counter 0 is never emitted.
    assert_eq!(counters[0], 1, "the first id is mwAQ");
    assert_eq!(real[0], counter_to_id(1));

    // Replaying the page with an allocator that knows about the collisions must
    // yield the same ids in the same order. `Zebra`'s holes are what make this a
    // real test of the skip rule rather than of the counter.
    assert!(
        !holes.is_empty(),
        "a real page has collision holes; finding none means the id filter is \
         accepting strings the generator never produced"
    );
    let mut colliding: Vec<String> = Vec::new();
    for (a, b) in &holes {
        for skipped in (a + 1)..*b {
            colliding.push(counter_to_id(skipped));
        }
    }
    let mut alloc = rustoid_core::pagebundle::NodeIdAllocator::new(colliding);
    let mut reproduced = Vec::with_capacity(real.len());
    for _ in 0..real.len() {
        reproduced.push(alloc.next_id());
    }
    assert_eq!(
        reproduced, real,
        "replaying with the collision ids must reproduce the page's ids exactly"
    );
}

/// The encoder and decoder must agree on every counter a real page uses.
#[test]
fn real_page_ids_round_trip() {
    let Some(html) = cached_zebra() else {
        eprintln!("skipped: no cached Parsoid HTML for Zebra");
        return;
    };
    for id in node_ids(&html) {
        let n = id_to_counter(&id).expect("decodable");
        assert_eq!(counter_to_id(n), id, "round trip failed for {id}");
    }
}
