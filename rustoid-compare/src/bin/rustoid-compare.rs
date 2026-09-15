//! `rustoid-compare` — compare rustoid against a live wiki's own Parsoid.
//!
//! ```text
//! # One page, reporting the first difference:
//! rustoid-compare --wiki en.wikipedia.org --page "UFC BJJ"
//!
//! # Offline re-run from cache (no network):
//! rustoid-compare --wiki en.wikipedia.org --page "UFC BJJ" --offline
//!
//! # Show what is cached, or drop it:
//! rustoid-compare --wiki en.wikipedia.org
//! rustoid-compare --wiki en.wikipedia.org --flush
//! rustoid-compare --flush-all
//! ```

use std::sync::{Arc, Mutex};

use clap::Parser;

use rustoid_compare::{EntryKind, Outcome, Wiki, WikiCache, WikiClient};

#[derive(Parser, Debug)]
#[command(name = "rustoid-compare")]
#[command(about = "Compare rustoid's output against a live wiki's Parsoid")]
struct Cli {
    /// Wiki host, e.g. `en.wikipedia.org`.
    #[arg(long, default_value = "en.wikipedia.org")]
    wiki: String,

    /// Page title to compare.
    #[arg(long)]
    page: Option<String>,

    /// Pin to this revision (default: the wiki's latest).
    #[arg(long)]
    revision: Option<u64>,

    /// Cache root. Defaults to `$RUSTOID_CACHE_DIR`, else `~/.cache/rustoid-compare`.
    #[arg(long)]
    cache_dir: Option<std::path::PathBuf>,

    /// Never hit the network; use the cache only.
    #[arg(long)]
    offline: bool,

    /// Re-fetch this page, ignoring cached entries.
    #[arg(long)]
    refresh: bool,

    /// Delete everything cached for this wiki, then exit.
    #[arg(long)]
    flush: bool,

    /// Delete every wiki's cache, then exit.
    #[arg(long)]
    flush_all: bool,

    /// Print the cached wikitext and stop (no comparison).
    #[arg(long)]
    show_wikitext: bool,

    /// Print both renderings when they differ.
    #[arg(long, short = 'v')]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let root = cli
        .cache_dir
        .clone()
        .unwrap_or_else(rustoid_compare::harness::default_cache_root);

    if cli.flush_all {
        WikiCache::flush_all(&root)?;
        println!("flushed all caches under {}", root.display());
        return Ok(());
    }

    let mut cache = WikiCache::open(&root, &cli.wiki)?;
    if cli.flush {
        cache.flush()?;
        println!("flushed cache for {}", cli.wiki);
        return Ok(());
    }

    let Some(page) = cli.page.clone() else {
        // No page: report what is cached, which is the useful no-argument action.
        println!(
            "cache: {} — {} entries for {}",
            cache.dir().display(),
            cache.len(),
            cli.wiki
        );
        if cache.is_empty() {
            eprintln!("nothing cached; pass --page <title> to compare a page");
        }
        return Ok(());
    };

    let wiki = Wiki::new(&cli.wiki);

    if cli.show_wikitext {
        let cached = cache.get(EntryKind::Page, &page)?;
        return match cached {
            Some(c) => {
                println!("{}", c.body);
                eprintln!(
                    "(cached; revid={:?}, fetched_at={:?})",
                    c.meta.revid, c.meta.fetched_at
                );
                Ok(())
            }
            None => {
                eprintln!("not cached: {page}");
                std::process::exit(2);
            }
        };
    }

    let client = Arc::new(WikiClient::new(wiki)?);
    let config = rustoid_core::mock::MockSiteConfig::new();
    let req = rustoid_compare::CompareRequest {
        title: page.clone(),
        revid: cli.revision,
        refresh: cli.refresh,
        offline: cli.offline,
    };

    let cache = Mutex::new(cache);
    let rt = tokio::runtime::Runtime::new()?;
    let comparison = rt.block_on(rustoid_compare::compare_page(
        &client, &config, &cache, &req,
    ))?;

    let status = match &comparison.outcome {
        Outcome::Match => "MATCH".to_string(),
        Outcome::Differ { detail } => format!("DIFFER\n{detail}"),
        Outcome::Skipped { reason } => format!("SKIP ({reason})"),
    };
    println!("{} @ r{} — {status}", comparison.title, comparison.revid);
    println!(
        "  parsoid {} bytes, rustoid {} bytes",
        comparison.parsoid_html.len(),
        comparison.rustoid_html.len()
    );
    if cli.verbose && !comparison.outcome.is_match() {
        println!("\n--- parsoid ---\n{}", comparison.parsoid_html);
        println!("\n--- rustoid ---\n{}", comparison.rustoid_html);
    }
    if !comparison.outcome.is_match() {
        std::process::exit(1);
    }
    Ok(())
}
