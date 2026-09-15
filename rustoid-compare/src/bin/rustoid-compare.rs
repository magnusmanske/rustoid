//! `rustoid-compare` — compare rustoid against a live wiki's own Parsoid.
//!
//! ```text
//! # One page, reporting the first difference:
//! rustoid-compare --wiki en.wikipedia.org --page "UFC BJJ"
//!
//! # The built-in corpus, with a scoreboard:
//! rustoid-compare --wiki en.wikipedia.org --corpus default
//!
//! # A custom corpus, offline, with per-failure detail:
//! rustoid-compare --wiki en.wikipedia.org --corpus my.txt --offline -v
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

use rustoid_compare::{
    Corpus, EntryKind, Outcome, Row, Scoreboard, Unexpanded, Wiki, WikiCache, WikiClient,
};

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

    /// Compare a corpus of pages. `default` uses the built-in corpus; otherwise
    /// this is a path to a corpus file (one `Title | tag, tag` per line).
    #[arg(long, value_name = "FILE_OR_DEFAULT")]
    corpus: Option<String>,

    /// Print the scoreboard only, without the per-page detail lines.
    #[arg(long)]
    quiet: bool,

    /// Pause this many milliseconds between page fetches, to stay well inside
    /// the wiki's rate limits on an uncached run.
    #[arg(long, default_value_t = 200)]
    delay_ms: u64,

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
    if cli.page.is_some() && cli.corpus.is_some() {
        return Err("--page and --corpus are mutually exclusive".into());
    }

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

    // A page or a corpus is required; with neither, report what is cached, which
    // is the useful no-argument action. In corpus mode the page stays empty and
    // is never used, because the corpus branch below returns first.
    let page = match cli.page.clone() {
        Some(page) => page,
        None if cli.corpus.is_some() => String::new(),
        None => {
            println!(
                "cache: {} — {} entries for {}",
                cache.dir().display(),
                cache.len(),
                cli.wiki
            );
            if cache.is_empty() {
                eprintln!("nothing cached; pass --page <title> or --corpus default");
            }
            return Ok(());
        }
    };

    let wiki = Wiki::new(&cli.wiki);

    if cli.show_wikitext {
        if page.is_empty() {
            return Err("--show-wikitext needs --page <title>".into());
        }
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
    let cache = Arc::new(Mutex::new(cache));
    let rt = tokio::runtime::Runtime::new()?;

    // The wiki's own configuration, not a hardcoded enwiki-shaped mock: a tag
    // list that omits a wiki's extensions would make them render as plain text.
    let config = rt.block_on(rustoid_compare::load_site_config(
        Some(&client),
        &cache,
        cli.offline,
        cli.refresh,
    ))?;
    if cli.verbose {
        println!(
            "siteinfo: {} extension tags, {} function hooks, {} magic words, {} namespaces, {} interwiki",
            config.extension_tag_count(),
            config.function_hook_count(),
            config.magic_word_count(),
            config.namespace_count(),
            config.interwiki_count(),
        );
    }

    if let Some(spec) = &cli.corpus {
        let corpus = if spec == "default" {
            Corpus::builtin()
        } else {
            Corpus::from_file(std::path::Path::new(spec))?
        };
        return run_corpus(cli, &client, &config, &cache, &rt, &corpus);
    }

    let req = rustoid_compare::CompareRequest {
        title: page.clone(),
        revid: cli.revision,
        refresh: cli.refresh,
        offline: cli.offline,
    };

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

/// Compare every entry in a corpus and print the scoreboard.
///
/// Pages are compared sequentially, not concurrently. The bottleneck is the
/// wiki's rate limiter, not the local work, so concurrency would buy little
/// while making a mid-run failure much harder to attribute.
fn run_corpus<C: rustoid_core::SiteConfig>(
    cli: &Cli,
    client: &Arc<WikiClient>,
    config: &C,
    cache: &Arc<Mutex<WikiCache>>,
    rt: &tokio::runtime::Runtime,
    corpus: &Corpus,
) -> Result<(), Box<dyn std::error::Error>> {
    if corpus.is_empty() {
        return Err(format!("corpus {} is empty", corpus.name).into());
    }

    let mut rows = Vec::with_capacity(corpus.len());
    for (n, entry) in corpus.entries.iter().enumerate() {
        let req = rustoid_compare::CompareRequest {
            title: entry.title.clone(),
            // A pinned revision only makes sense for a single page; in a corpus
            // each page resolves its own, and the cache pins it thereafter.
            revid: None,
            refresh: cli.refresh,
            offline: cli.offline,
        };
        // Progress always goes to stderr, even under `--quiet`: `--quiet` means
        // "scoreboard only" for stdout, not "show nothing". A cold corpus run is
        // network-bound and can take minutes per page, and a run that looks
        // frozen is indistinguishable from one that has hung.
        eprint!("[{}/{}] {} … ", n + 1, corpus.len(), entry.title);

        let comparison = rt.block_on(rustoid_compare::compare_page(client, config, cache, &req));

        // A single failing page must not abort the run: a corpus is exactly the
        // case where some pages are known-bad, and losing the other 30 results
        // to one error would defeat the point.
        let row = match comparison {
            Ok(c) => Row {
                title: entry.title.clone(),
                tags: entry.tags.iter().cloned().collect(),
                revid: Some(c.revid),
                parsoid_bytes: c.parsoid_html.len(),
                rustoid_bytes: c.rustoid_html.len(),
                unexpanded_rustoid: c.unexpanded_rustoid,
                unexpanded_parsoid: c.unexpanded_parsoid,
                outcome: c.outcome,
            },
            Err(e) => Row {
                title: entry.title.clone(),
                tags: entry.tags.iter().cloned().collect(),
                revid: None,
                parsoid_bytes: 0,
                rustoid_bytes: 0,
                unexpanded_rustoid: Unexpanded::default(),
                unexpanded_parsoid: Unexpanded::default(),
                outcome: Outcome::Skipped {
                    reason: e.to_string(),
                },
            },
        };
        // The category closes the progress line opened above. Under `--quiet`
        // that is all that is printed per page; otherwise the title is repeated
        // so a scrollback reads as a list.
        if cli.quiet {
            eprintln!("{}", row.category());
        } else {
            eprintln!(
                "[{}/{}] {} — {}",
                n + 1,
                corpus.len(),
                entry.title,
                row.category()
            );
        }
        rows.push(row);

        // Only pace the run when we might actually hit the network.
        if cli.delay_ms > 0 && !cli.offline && n + 1 < corpus.len() {
            std::thread::sleep(std::time::Duration::from_millis(cli.delay_ms));
        }
    }

    let board = Scoreboard::new(corpus.name.clone(), rows);
    print!("{}", board.render(cli.verbose));

    // The cache is written through on every put, but flushing makes the
    // manifest on-disk state match what the run reported.
    cache
        .lock()
        .map_err(|_| "cache mutex poisoned")?
        .write_index()?;

    if board.compared() == 0 {
        return Err("nothing could be compared (all pages skipped)".into());
    }
    Ok(())
}
