//! `crawler` CLI.
//!
//! Runs on a current-thread Tokio runtime: the SQLite connection is not `Send`,
//! and a poller has no need for a multi-threaded work-stealing runtime anyway.

mod service;

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use clap::{Parser, Subcommand};
use crawler_core::{
    build_epub, default_db_path, epub_path, profiles, sync_novel, GenericSource, ReqwestFetcher,
    Source, Store, StoredNovel, StoredSource,
};

/// Days a Live+Completed novel must be quiet before it's judged LikelyComplete.
/// Will become configurable via config.ini in a later slice.
const DEFAULT_QUIET_GRACE_DAYS: u32 = 30;

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
    /// Add an alternate (fallback) source to an existing subscription.
    AddSource {
        /// Existing novel id or title.
        novel: String,
        /// New source URL for the same novel.
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
    /// Sync all subscriptions (download new chapters). This is what the
    /// background task runs.
    Sync {
        /// Max new chapters per novel this run (0 = all missing).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
    /// Purge exported chapters of completed novels to free space (never touches
    /// un-exported chapters or ongoing novels).
    Prune {
        /// Keep exported chapters this many days before purging (0 = purge now).
        #[arg(long, default_value_t = 30)]
        retention_days: u32,
    },
    /// Manage the background sync task (install/uninstall/status).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// List a novel's discovered chapters (walks the full ToC; no DB, no bodies).
    List {
        url: String,
        #[arg(long, default_value_t = 1500)]
        delay_ms: u64,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the background sync task (Windows Task Scheduler).
    Install {
        /// How often to run the sync, in minutes.
        #[arg(long, default_value_t = 60)]
        interval_minutes: u32,
    },
    /// Remove the background sync task.
    Uninstall,
    /// Show whether the background sync task is installed.
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Subscribe { url, delay_ms } => subscribe(url, delay_ms).await,
        Command::AddSource {
            novel,
            url,
            delay_ms,
        } => add_source(novel, url, delay_ms).await,
        Command::Subs => subs(),
        Command::Unsubscribe { novel } => unsubscribe(novel),
        Command::Fetch {
            novel,
            limit,
            delay_ms,
        } => fetch(novel, limit, delay_ms).await,
        Command::Export { novel, out } => export(novel, out),
        Command::Prune { retention_days } => prune(retention_days),
        Command::Sync { limit, delay_ms } => sync_all(limit, delay_ms).await,
        Command::Service { action } => match action {
            ServiceAction::Install { interval_minutes } => service_install(interval_minutes),
            ServiceAction::Uninstall => service_uninstall(),
            ServiceAction::Status => service_status(),
        },
        Command::List { url, delay_ms } => list(url, delay_ms).await,
    }
}

/// Build a source adapter per stored source, in priority order (primary first),
/// skipping any source whose host we have no adapter for.
fn build_sources(
    novel: &StoredNovel,
    delay_ms: u64,
) -> Result<Vec<(StoredSource, Box<dyn Source>)>> {
    let mut sources = Vec::new();
    for s in &novel.sources {
        match profiles::for_url(&s.url) {
            Some(profile) => {
                let fetcher = ReqwestFetcher::new(Duration::from_millis(delay_ms))?;
                sources.push((
                    s.clone(),
                    Box::new(GenericSource::new(profile, fetcher)) as Box<dyn Source>,
                ));
            }
            None => eprintln!("  (skipping source {} — no adapter for its host)", s.url),
        }
    }
    Ok(sources)
}

/// Acquire the single-instance sync lock. Returns `None` if another sync holds
/// it (the caller should then skip, not error). The lock releases when the
/// returned file handle is dropped / the process exits.
fn acquire_sync_lock() -> Result<Option<File>> {
    let lock_path = default_db_path()?
        .parent()
        .map(|p| p.join("sync.lock"))
        .ok_or_else(|| anyhow!("cannot resolve lock path"))?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0) => exclusive: a second opener gets a sharing violation.
        match OpenOptions::new()
            .write(true)
            .create(true)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(32) => Ok(None), // ERROR_SHARING_VIOLATION
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(windows))]
    {
        // Best-effort placeholder until a real advisory lock is added.
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&lock_path)?;
        Ok(Some(f))
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

async fn add_source(novel: String, url: String, delay_ms: u64) -> Result<()> {
    let store = Store::open_default()?;
    let found = store
        .find_novel(&novel)?
        .ok_or_else(|| anyhow!("no subscription matches \"{novel}\""))?;

    // Building the source validates that we have an adapter for the URL's host.
    let source = source_for(&url, delay_ms)?;

    // Sanity check: fetch the new source's title and warn if it differs — the
    // user already confirmed by running this command, so we proceed regardless.
    eprintln!("Checking new source...");
    if let Ok(meta) = source.fetch_novel(&url).await {
        if !meta.title.eq_ignore_ascii_case(&found.title) {
            eprintln!(
                "  warning: this source's title is \"{}\", but the novel is \"{}\". \
                 Proceeding since you asked to link them.",
                meta.title, found.title
            );
        }
    }

    let sid = store.add_source(found.id, source.name(), &url)?;
    println!(
        "Added {} as a fallback source (#{sid}) for \"{}\".",
        source.name(),
        found.title
    );
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

    let sources = build_sources(&found, delay_ms)?;
    ensure!(!sources.is_empty(), "no usable sources for \"{}\"", found.title);

    let before = store.stored_chapter_numbers(found.id)?.len();
    eprintln!("Syncing \"{}\" from {} source(s)...", found.title, sources.len());

    let report = sync_novel(
        &store,
        found.id,
        found.derived_state,
        &sources,
        limit,
        |line| eprintln!("  {line}"),
    )
    .await?;

    let final_state = store.reevaluate_completion(found.id, DEFAULT_QUIET_GRACE_DAYS)?;
    if final_state != found.derived_state {
        eprintln!("  state: {} -> {}", found.derived_state.as_str(), final_state.as_str());
    }

    for w in &report.warnings {
        eprintln!("  ! {w}");
    }
    if !report.failures.is_empty() {
        eprintln!(
            "  ! {} chapter(s) could not be fetched from any source: {:?}",
            report.failures.len(),
            report.failures
        );
    }

    let fallback_note = if report.from_fallback > 0 {
        format!(" ({} from fallback sources)", report.from_fallback)
    } else {
        String::new()
    };
    let mode = if report.delta_mode { "delta check" } else { "full scan" };
    println!(
        "Fetched {} new chapters for \"{}\"{fallback_note} (now {} stored) [{mode}].",
        report.newly_fetched,
        found.title,
        before + report.newly_fetched as usize
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

async fn sync_all(limit: usize, delay_ms: u64) -> Result<()> {
    // Single-instance guard: overlapping scheduled runs should skip, not stack.
    let _lock = match acquire_sync_lock()? {
        Some(f) => f,
        None => {
            eprintln!("Another sync is already running; skipping.");
            return Ok(());
        }
    };

    let store = Store::open_default()?;
    let novels = store.list_subscriptions()?;
    if novels.is_empty() {
        println!("No subscriptions to sync.");
        return Ok(());
    }

    let mut total_new = 0u32;
    for novel in &novels {
        let sources = match build_sources(novel, delay_ms) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                eprintln!("[{}] no usable sources; skipping", novel.title);
                continue;
            }
            Err(e) => {
                eprintln!("[{}] {e}; skipping", novel.title);
                continue;
            }
        };
        eprintln!("Syncing \"{}\"...", novel.title);
        // One novel's failure must not abort the whole run.
        match sync_novel(
            &store,
            novel.id,
            novel.derived_state,
            &sources,
            limit,
            |line| eprintln!("  {line}"),
        )
        .await
        {
            Ok(report) => {
                for w in &report.warnings {
                    eprintln!("  ! {w}");
                }
                let final_state = store
                    .reevaluate_completion(novel.id, DEFAULT_QUIET_GRACE_DAYS)
                    .unwrap_or(report.new_state);
                if final_state != novel.derived_state {
                    eprintln!("  {} state: {} -> {}", novel.title, novel.derived_state.as_str(), final_state.as_str());
                }
                total_new += report.newly_fetched;
                let mode = if report.delta_mode { "delta" } else { "full" };
                println!("  {}: +{} new chapters [{mode}]", novel.title, report.newly_fetched);
            }
            Err(e) => eprintln!("  ! sync failed for \"{}\": {e}", novel.title),
        }
    }
    println!(
        "Sync complete: {total_new} new chapter(s) across {} novel(s).",
        novels.len()
    );
    Ok(())
}

fn prune(retention_days: u32) -> Result<()> {
    let store = Store::open_default()?;
    let n = store.apply_retention(retention_days)?;
    println!("Pruned {n} exported chapter(s) from completed novels (retention {retention_days}d).");
    Ok(())
}

fn service_install(interval_minutes: u32) -> Result<()> {
    let exe = std::env::current_exe()?;
    service::manager()?.install(&exe, interval_minutes)?;
    println!(
        "Installed background sync: \"{}\" runs `\"{}\" sync` every {interval_minutes} min.",
        service::TASK_NAME,
        exe.display()
    );
    println!("Note: it runs in your user session; a brief console window may appear each run.");
    Ok(())
}

fn service_uninstall() -> Result<()> {
    service::manager()?.uninstall()?;
    println!("Removed the background sync task.");
    Ok(())
}

fn service_status() -> Result<()> {
    let st = service::manager()?.status()?;
    if st.installed {
        println!("Background sync is INSTALLED.\n{}", st.detail);
    } else {
        println!("Background sync is not installed. Install with `crawler service install`.");
    }
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
