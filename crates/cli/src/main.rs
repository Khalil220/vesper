//! `crawler` CLI. First slice: a single `export` command that drives the whole
//! fetch -> extract -> package pipeline end to end.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use crawler_core::{
    build_epub, profiles, sanitize_filename, GenericSource, ReqwestFetcher, Source,
};

#[derive(Parser)]
#[command(name = "crawler", about = "Webnovel crawler -> EPUB", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch a novel (or its first N chapters) and export to an EPUB.
    Export {
        /// Novel landing-page URL.
        url: String,
        /// Number of chapters to fetch (0 = all).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Output EPUB path (default: "<title>.epub" in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Politeness delay between requests, in milliseconds.
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Export {
            url,
            limit,
            out,
            delay_ms,
        } => export(url, limit, out, delay_ms).await,
    }
}

async fn export(url: String, limit: usize, out: Option<PathBuf>, delay_ms: u64) -> Result<()> {
    let fetcher = ReqwestFetcher::new(Duration::from_millis(delay_ms))?;
    let source = GenericSource::new(profiles::novgo(), fetcher);

    ensure!(
        source.matches(&url),
        "no known source handles this URL: {url}"
    );

    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;
    eprintln!("  Title:  {}", meta.title);
    if let Some(author) = &meta.author {
        eprintln!("  Author: {author}");
    }
    eprintln!("  Status hint: {:?} (hint only)", meta.status_hint);

    let needed = (limit != 0).then_some(limit);
    eprintln!("Discovering chapters...");
    let mut refs = source.discover_chapters(&url, needed).await?;
    if let Some(n) = needed {
        refs.truncate(n);
    }
    ensure!(!refs.is_empty(), "no chapters found for {url}");
    eprintln!("  {} chapters to fetch.", refs.len());

    let mut chapters = Vec::with_capacity(refs.len());
    for (i, cref) in refs.iter().enumerate() {
        eprintln!("[{}/{}] ch.{} {}", i + 1, refs.len(), cref.number, cref.title);
        chapters.push(source.fetch_chapter(cref).await?);
    }

    let out_path = out.unwrap_or_else(|| PathBuf::from(format!("{}.epub", sanitize_filename(&meta.title))));
    build_epub(&meta, &chapters, &out_path)?;
    eprintln!("Wrote {}", out_path.display());
    Ok(())
}
