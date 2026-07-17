//! `crawler` CLI. Drives the fetch -> extract -> package pipeline.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use crawler_core::{
    build_epub, epub_path, profiles, GenericSource, ReqwestFetcher, Source,
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
        /// Output EPUB path. Default: <library>/<author>/<novel>/<novel>.epub,
        /// where <library> is Documents/lightnovels.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Base politeness delay between requests, in milliseconds.
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
    /// List a novel's discovered chapters (walks the full table of contents).
    List {
        /// Novel landing-page URL.
        url: String,
        /// Base politeness delay between requests, in milliseconds.
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
        Command::List { url, delay_ms } => list(url, delay_ms).await,
    }
}

/// Build the source adapter for a URL, erroring if no known source handles it.
fn source_for(url: &str, delay_ms: u64) -> Result<GenericSource<ReqwestFetcher>> {
    let fetcher = ReqwestFetcher::new(Duration::from_millis(delay_ms))?;
    let source = GenericSource::new(profiles::novgo(), fetcher);
    ensure!(source.matches(url), "no known source handles this URL: {url}");
    Ok(source)
}

/// Default library root: `<Documents>/lightnovels`, falling back to
/// `./lightnovels` if the Documents folder can't be resolved.
fn default_library() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(|d| d.join("lightnovels")))
        .unwrap_or_else(|| PathBuf::from("lightnovels"))
}

async fn export(url: String, limit: usize, out: Option<PathBuf>, delay_ms: u64) -> Result<()> {
    let source = source_for(&url, delay_ms)?;

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

    let out_path = out.unwrap_or_else(|| {
        epub_path(&default_library(), meta.author.as_deref(), &meta.title, None)
    });
    build_epub(&meta, &chapters, &out_path)?;
    eprintln!("Wrote {}", out_path.display());
    Ok(())
}

async fn list(url: String, delay_ms: u64) -> Result<()> {
    let source = source_for(&url, delay_ms)?;

    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;
    eprintln!("  Title:  {}", meta.title);
    if let Some(author) = &meta.author {
        eprintln!("  Author: {author}");
    }

    eprintln!("Walking full table of contents...");
    let refs = source.discover_chapters(&url, None).await?;
    ensure!(!refs.is_empty(), "no chapters found for {url}");

    println!("{} chapters discovered.", refs.len());
    if let (Some(first), Some(last)) = (refs.first(), refs.last()) {
        println!("  first: ch.{} {}", first.number, first.title);
        println!("  last:  ch.{} {}", last.number, last.title);
    }
    // Contiguity check: warn if the discovered numbers have gaps.
    let expected_last = refs.len() as u32;
    if let Some(last) = refs.last() {
        if last.number != expected_last {
            println!(
                "  note: highest chapter number is {} but {} chapters were found (numbering gaps or extras).",
                last.number, expected_last
            );
        }
    }
    Ok(())
}
