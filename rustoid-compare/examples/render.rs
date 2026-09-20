//! Render a wikitext snippet offline through the cached wiki data.
//!
//! A diagnostic for the expansion blow-up: the whole `Zebra` page takes minutes
//! to expand, which makes iterating on the cause impractical. This reduces the
//! input to whatever reproduces, using the same `WikiSiteConfig` and cache as a
//! real comparison run.
//!
//! Usage: `RUSTOID_CACHE_DIR=… cargo run --example render -- <title> <file> [online]`
//!
//! Without `online` the run is offline: a cache miss is a miss, and a template
//! that was never fetched renders as a redlink. Pass `online` to let a miss be
//! fetched and cached, which is how a snippet can be made reproducible.

use std::sync::{Arc, Mutex};

use rustoid_compare::cache::WikiCache;
use rustoid_compare::harness::CachedDataSource;
use rustoid_compare::siteconfig::WikiSiteConfig;
use rustoid_compare::wire::{Wiki, WikiClient};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let title = args.get(1).cloned().unwrap_or_else(|| "Test".to_string());
    let path = args.get(2).expect("usage: render <title> <file> [online]");
    let online = args.iter().any(|a| a == "online");
    let wikitext = std::fs::read_to_string(path).expect("read input");

    let root = std::env::var("RUSTOID_CACHE_DIR").expect("RUSTOID_CACHE_DIR");
    let mut cache = WikiCache::open(&root, "en.wikipedia.org").expect("open cache");
    if cache.is_empty() {
        cache.reindex().expect("reindex");
    }
    let siteinfo = cache
        .get(rustoid_compare::EntryKind::SiteInfo, "siteinfo")
        .expect("read siteinfo")
        .expect("siteinfo present")
        .body;
    let config = WikiSiteConfig::from_siteinfo_json(&siteinfo).expect("parse siteinfo");

    let client = if online {
        Some(Arc::new(
            WikiClient::new(Wiki::new("en.wikipedia.org")).expect("client"),
        ))
    } else {
        None
    };
    let source = CachedDataSource::new(client, Arc::new(Mutex::new(cache)), !online);
    let parser = rustoid_core::Parser::new(&config);
    let options = rustoid_core::ParserOptions {
        node_ids: true,
        strip_data_parsoid: true,
        ..rustoid_core::ParserOptions::for_page(&title)
    };
    let started = std::time::Instant::now();
    let html = parser
        .wikitext_to_html_expanded(&wikitext, &source, &options)
        .await
        .expect("parse");
    source.flush().expect("flush cache");
    eprintln!(
        "rendered {} bytes in {:.1}s",
        html.len(),
        started.elapsed().as_secs_f64()
    );
    print!("{html}");
}
