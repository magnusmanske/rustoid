//! Rustoid CLI — command-line interface for the Parsoid-compatible parser.
//!
//! Usage:
//!   rustoid render --page "Main Page"
//!   rustoid render --page "Foo" --format json
//!   rustoid roundtrip --page "Foo"
//!   rustoid test --file tests/fixtures/parserTests.txt
//!   rustoid serve --api-url https://en.wikipedia.org/w/api.php

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
#[command(name = "rustoid")]
#[command(version = "0.1.0")]
#[command(about = "A Rust implementation of the Parsoid MediaWiki parser")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Parse a page and render it to the specified format.
    Render {
        /// Page title to render.
        #[arg(short, long)]
        page: String,

        /// Output format: html, json.
        #[arg(short, long, default_value = "html")]
        format: String,

        /// MediaWiki API URL for fetching page content.
        #[arg(long, default_value = "https://en.wikipedia.org/w/api.php")]
        api_url: String,
    },

    /// Run a round-trip test: wikitext → HTML → wikitext.
    Roundtrip {
        /// Page title to round-trip.
        #[arg(short, long)]
        page: String,

        /// MediaWiki API URL for fetching page content.
        #[arg(long, default_value = "https://en.wikipedia.org/w/api.php")]
        api_url: String,
    },

    /// Run the Parsoid parser test suite.
    Test {
        /// Path to a parserTests.txt file.
        #[arg(short, long)]
        file: PathBuf,

        /// Filter tests by name substring.
        #[arg(long)]
        filter: Option<String>,

        /// Only run tests for the specified mode (wt2html, html2wt, selser, wt2wt).
        #[arg(long)]
        mode: Option<String>,
    },

    /// Start a local HTTP server mimicking the Parsoid REST API.
    Serve {
        /// MediaWiki API URL for fetching page content.
        #[arg(long, default_value = "https://en.wikipedia.org/w/api.php")]
        api_url: String,

        /// Port to listen on.
        #[arg(short, long, default_value = "8000")]
        port: u16,
    },
}

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Render {
            page,
            format,
            api_url,
        } => {
            info!("Rendering page: {page} as {format}");
            println!("<!-- Rendering {page} from {api_url} as {format} -->");
            match format.as_str() {
                "html" => {
                    println!("<p>Phase 12: HTML rendering not yet implemented.</p>");
                }
                "json" => {
                    println!("{{\"status\": \"JSON rendering not yet implemented\"}}");
                }
                _ => {
                    eprintln!("Unknown format: {format}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Roundtrip { page, api_url } => {
            info!("Round-tripping page: {page} from {api_url}");
            println!("<!-- Round-trip for {page} not yet implemented -->");
        }

        Commands::Test { file, filter, mode } => {
            info!("Running test file: {file:?} (filter: {filter:?}, mode: {mode:?})");
            println!("Test harness not yet implemented.");
            println!("Test file: {file:?}");
        }

        Commands::Serve { api_url, port } => {
            info!("Starting server on port {port}, backed by {api_url}");
            println!("Server mode not yet implemented.");
            println!("Would serve on http://localhost:{port}/");
        }
    }
}
