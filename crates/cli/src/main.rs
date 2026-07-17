//! `crawler` CLI.
//!
//! Runs on a current-thread Tokio runtime: the SQLite connection is not `Send`,
//! and a poller has no need for a multi-threaded work-stealing runtime anyway.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use clap::{Parser, Subcommand};
use crawler_core::{
    build_epub, epub_path, profiles, GenericSource, ReqwestFetcher, Source, Store,
};

#[derive(Parser)]
#[command(name = "crawler", about = "Webnovel crawler -> EPUB", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to a novel (registers it and its primary source).
    Subscribe {
        /// Novel landing-page URL.
        url: String,
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
    /// List all subscriptions.
    Subs,
    /// Remove a subscription and its downloaded chapters.
    Unsubscribe {
        /// Novel id or title.
        novel: String,
    },
    /// Download missing chapters for a subscribed novel into the library (resume-aware).
    Fetch {
        /// Novel id or title.
        novel: String,
        /// Max new chapters to fetch this run (0 = all missing).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
    /// Build an EPUB from a subscribed novel's stored chapters.
    Export {
        /// Novel id or title.
        novel: String,
        /// Output EPUB path (default: <Documents>/lightnovels/<author>/<novel>/<novel>.epub).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List a novel's discovered chapters (walks the full ToC; no DB, no bodies).
    List {
        url: String,
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Subscribe { url, delay_ms } => subscribe(url, delay_ms).await,
        Command::Subs => subs(),
        Command::Unsubscribe { novel } => unsubscribe(novel),
        Command::Fetch {
            novel,
            limit,
            delay_ms,
        } => fetch(novel, limit, delay_ms).await,
        Command::Export { novel, out } => export(novel, out),
        Command::List { url, delay_ms } => list(url, delay_ms).await,
    }
}

/// Build the source adapter for a URL, erroring if no known source handles it.
fn source_for(url: &str, delay_ms: u64) -> Result<GenericSource<ReqwestFetcher>> {
    let profile =
        profiles::for_url(url).ok_or_else(|| anyhow!("no known source handles this URL: {url}"))?;
    let fetcher = ReqwestFetcher::new(Duration::from_millis(delay_ms))?;
    Ok(GenericSource::new(profile, fetcher))
}

/// Default library root: `<Documents>/lightnovels`, falling back to
/// `./lightnovels` if the Documents folder can't be resolved.
fn default_library() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(|d| d.join("lightnovels")))
        .unwrap_or_else(|| PathBuf::from("lightnovels"))
}

async fn subscribe(url: String, delay_ms: u64) -> Result<()> {
    let source = source_for(&url, delay_ms)?;
    eprintln!("Fetching novel metadata...");
    let meta = source.fetch_novel(&url).await?;

    let store = Store::open_default()?;
    let id = store.subscribe(&meta, source.name())?;

    println!("Subscribed to \"{}\" (novel #{id}).", meta.title);
    if let Some(author) = &meta.author {
        println!("  Author: {author}");
    }
    println!("  Source: {} ({url})", source.name());
    println!("Next: `crawler fetch {id}` to download chapters.");
    Ok(())
}

fn subs() -> Result<()> {
    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    if novels.is_empty() {
        println!("No subscriptions yet. Add one with `crawler subscribe <url>`.");
        return Ok(());
    }
    for n in novels {
        let author = n.author.as_deref().unwrap_or("Unknown Author");
        println!(
            "#{}  {} — {}  [{} chapters, {}]",
            n.id,
            n.title,
            author,
            n.chapter_count,
            n.derived_state.as_str()
        );
        for s in &n.sources {
            let seen = s
                .last_seen_chapter
                .map(|c| format!("last seen ch.{c}"))
                .unwrap_or_else(|| "not yet synced".into());
            let role = if s.priority == 1 { "primary" } else { "fallback" };
            println!("    [{role}] {} — {} ({seen})", s.name, s.url);
        }
    }
    Ok(())
}

fn unsubscribe(novel: String) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    store.remove_subscription(found.id)?;
    println!(
        "Unsubscribed from \"{}\" (novel #{}); removed {} stored chapters.",
        found.title, found.id, found.chapter_count
    );
    Ok(())
}

async fn fetch(novel: String, limit: usize, delay_ms: u64) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;
    let primary = found
        .primary_source()
        .ok_or_else(|| anyhow!("novel #{} has no source", found.id))?
        .clone();

    let source = source_for(&primary.url, delay_ms)?;

    eprintln!("Discovering chapters for \"{}\"...", found.title);
    let refs = source.discover_chapters(&primary.url, None).await?;
    ensure!(!refs.is_empty(), "no chapters found at {}", primary.url);

    let have = store.stored_chapter_numbers(found.id)?;
    let missing: Vec<_> = refs.iter().filter(|r| !have.contains(&r.number)).collect();
    let to_fetch: Vec<_> = if limit == 0 {
        missing
    } else {
        missing.into_iter().take(limit).collect()
    };

    eprintln!(
        "  {} discovered, {} already stored, {} to fetch.",
        refs.len(),
        have.len(),
        to_fetch.len()
    );
    if to_fetch.is_empty() {
        println!("Already up to date ({} chapters).", have.len());
        return Ok(());
    }

    let mut fetched = 0u32;
    let mut highest = primary.last_seen_chapter.unwrap_or(0);
    for (i, cref) in to_fetch.iter().enumerate() {
        eprintln!("[{}/{}] ch.{} {}", i + 1, to_fetch.len(), cref.number, cref.title);
        let chapter = source
            .fetch_chapter(cref)
            .await
            .with_context(|| format!("fetching chapter {}", cref.number))?;
        if store.insert_chapter_if_absent(found.id, primary.id, &chapter)? {
            fetched += 1;
        }
        highest = highest.max(cref.number);
    }
    store.update_source_progress(primary.id, highest)?;

    println!(
        "Fetched {fetched} new chapters for \"{}\" (now {} stored).",
        found.title,
        have.len() + fetched as usize
    );
    Ok(())
}

fn export(novel: String, out: Option<PathBuf>) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;

    let chapters = store.load_chapters(found.id)?;
    ensure!(
        !chapters.is_empty(),
        "\"{}\" has no downloaded chapters yet; run `crawler fetch {}` first",
        found.title,
        found.id
    );

    let meta = found.to_meta();
    let out_path = out.unwrap_or_else(|| {
        epub_path(&default_library(), meta.author.as_deref(), &meta.title, None)
    });
    build_epub(&meta, &chapters, &out_path)?;
    store.mark_all_exported(found.id)?;

    println!(
        "Exported {} chapters of \"{}\" to {}",
        chapters.len(),
        found.title,
        out_path.display()
    );
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
    Ok(())
}
