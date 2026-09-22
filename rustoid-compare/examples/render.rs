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
    let cache = Arc::new(Mutex::new(cache));
    let started = std::time::Instant::now();
    // The render is capped, because the whole point of this example is diagnosing
    // an expansion that does not terminate: without a cap it hangs forever instead
    // of reporting, which is the state that made the corpus unusable.
    //
    // It also runs on its own thread and outlives nothing — the process exits the
    // moment the cap fires — because `timeout` only cancels at an `.await` point,
    // and a non-terminating expansion never yields. Both the parser and the data
    // source are built *inside* the worker: `Parser` borrows `config` and is not
    // `Sync`, so neither can be moved in from here.
    let cap = std::env::var("RUSTOID_PAGE_STALL_SECS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    let wrap_sections = std::env::var_os("RUSTOID_WRAP_SECTIONS").is_some();
    let worker = tokio::task::spawn_blocking(move || {
        let source = CachedDataSource::new(client, cache, !online);
        let options = rustoid_core::ParserOptions {
            node_ids: true,
            strip_data_parsoid: true,
            wrap_sections,
            ..rustoid_core::ParserOptions::for_page(&title)
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let parser = rustoid_core::Parser::new(&config);
        let html = rt
            .block_on(parser.wikitext_to_html_expanded(&wikitext, &source, &options))
            .expect("parse");
        source.flush().expect("flush cache");
        html
    });
    let html = match tokio::time::timeout(std::time::Duration::from_secs_f64(cap), worker).await {
        Ok(Ok(html)) => html,
        Ok(Err(e)) => panic!("render task failed: {e}"),
        Err(_elapsed) => {
            eprintln!("STALLED: no render after {cap:.0}s");
            std::process::exit(2);
        }
    };
    eprintln!(
        "rendered {} bytes in {:.1}s",
        html.len(),
        started.elapsed().as_secs_f64()
    );
    print!("{html}");
}
