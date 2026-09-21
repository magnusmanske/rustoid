//! Populate the cache with pages a render needs.
//!
//! Two shapes of walk are covered, both of which an offline render needs to
//! terminate:
//!
//! - **A taxon's ancestry.** `Module:Autotaxobox` walks a taxon's parents by
//!   asking each `Template:Taxonomy/<taxon>` for its `parent` field, so a missing
//!   link makes the module read the *missing-template answer* as the next taxon
//!   and recurse on a title that grows every round. Titles are read from the
//!   taxonomy templates themselves, so this follows the module rather than
//!   guessing.
//! - **A module's transclusion targets.** A module concatenates what
//!   `frame:expandTemplate` gives it, so a template it calls that is not cached
//!   contributes a redlink where real markup belongs.
//!
//! Every fetch goes through the paced client and the cache, so a re-run costs
//! nothing.
//!
//! Usage:
//!   `… --example populate_taxonomy -- taxon <taxon>`
//!   `… --example populate_taxonomy -- titles <Title> <Title> …`

use std::sync::{Arc, Mutex};

use rustoid_compare::cache::{EntryKind, EntryMeta, WikiCache};
use rustoid_compare::wire::{Wiki, WikiClient};

/// The `|parent =` value of a taxonomy template body, if it names one.
fn parent_of(body: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim().trim_start_matches('|').trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("parent") {
            continue;
        }
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = std::env::var("RUSTOID_CACHE_DIR").expect("RUSTOID_CACHE_DIR");
    let client = WikiClient::new(Wiki::new("en.wikipedia.org")).expect("client");
    let cache = Arc::new(Mutex::new(
        WikiCache::open(&root, "en.wikipedia.org").expect("open cache"),
    ));

    match args.get(1).map(String::as_str) {
        Some("taxon") => {
            let start = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "Equus (Hippotigris)".to_string());
            populate_taxon(&client, &cache, start).await;
        }
        Some("titles") => {
            for title in &args[2..] {
                populate_title(&client, &cache, title).await;
            }
        }
        _ => {
            eprintln!("usage: populate_taxonomy taxon <taxon> | titles <Title> …");
            std::process::exit(2);
        }
    }

    cache
        .lock()
        .expect("cache lock")
        .write_index()
        .expect("index");
    println!("done");
}

/// Fetch `title` unless it is already cached. Returns the body either way.
async fn fetch_or_get(
    client: &WikiClient,
    cache: &Arc<Mutex<WikiCache>>,
    title: &str,
) -> Option<(String, bool)> {
    let cached = {
        let guard = cache.lock().expect("cache lock");
        guard.get(EntryKind::Template, title).expect("cache get")
    };
    if let Some(hit) = cached {
        return Some((hit.body, false));
    }
    let revid = client.latest_revid(title).await.expect("revid")?;
    let body = client.wikitext_at(revid).await.expect("wikitext");
    cache
        .lock()
        .expect("cache lock")
        .put(
            EntryKind::Template,
            title,
            &body,
            EntryMeta {
                kind: EntryKind::Template,
                title: title.to_string(),
                revid: Some(revid),
                fetched_at: None,
            },
        )
        .expect("cache put");
    println!("  fetched {title} r{revid}");
    Some((body, true))
}

/// Fetch one title, reporting whether it was already cached or is absent.
async fn populate_title(client: &WikiClient, cache: &Arc<Mutex<WikiCache>>, title: &str) {
    match fetch_or_get(client, cache, title).await {
        Some((_, false)) => println!("  {title} (cached)"),
        Some((_, true)) => {}
        None => println!("  {title} MISSING on the wiki"),
    }
}

/// Walk `taxon`'s ancestry, fetching each `Template:Taxonomy/<taxon>`.
async fn populate_taxon(client: &WikiClient, cache: &Arc<Mutex<WikiCache>>, start: String) {
    let mut taxon = start;
    let mut seen = std::collections::HashSet::new();
    // `MaxSearchLevels` in the module bounds the walk; the guard here is only to
    // stop a malformed chain from looping forever.
    for step in 0..80 {
        if !seen.insert(taxon.clone()) {
            println!("cycle at {taxon:?}; stopping");
            break;
        }
        let title = format!("Template:Taxonomy/{taxon}");
        let Some((body, was_fetched)) = fetch_or_get(client, cache, &title).await else {
            println!("{step:2} {title}  MISSING on the wiki; stopping");
            break;
        };
        if !was_fetched {
            println!("{step:2} {title}  (cached)");
        }
        match parent_of(&body) {
            Some(parent) => taxon = parent,
            None => {
                println!("{step:2} {title}  has no parent; chain complete");
                break;
            }
        }
    }
}
